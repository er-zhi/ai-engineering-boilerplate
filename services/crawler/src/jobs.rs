// Crawl jobs: rows in crawler.crawl_jobs, run under a concurrency cap, with page counts kept current as pages are stored and changed pages handed to knowledge-base.

use std::sync::Arc;

use chrono::Utc;
use common::proto::crawler::v1::CrawlStatus;
use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::{Expr, ExprTrait};
use sea_orm::{ActiveModelTrait, ColumnTrait, DatabaseConnection, DbErr, EntityTrait, QueryFilter};
use tokio::sync::{Semaphore, mpsc};

use crate::crawl::{self, CountedPage, FetchedPage, Limits, Unreachable};
use crate::entity::crawl_job::{self, Status};
use crate::graph::EdgeStore;
use crate::knowledge_base::KnowledgeBase;
use crate::links;
use crate::scope::Scope;
use crate::store::PageStore;

const JOB_ID_PREFIX: &str = "job-";

#[derive(Clone, Debug, PartialEq)]
pub struct CrawlJob {
    pub id: i64,
    pub base_url: String,
    pub status: CrawlStatus,
    pub pages_crawled: u32,
    pub pages_skipped: u32,
}

impl CrawlJob {
    pub fn public_id(&self) -> String {
        format!("{JOB_ID_PREFIX}{}", self.id)
    }
}

pub fn parse_job_id(public_id: &str) -> Option<i64> {
    public_id.strip_prefix(JOB_ID_PREFIX)?.parse().ok()
}

pub trait JobStore: Clone + Send + Sync + 'static {
    fn create(
        &self,
        base_url: &str,
        idempotency_key: Option<&str>,
    ) -> impl Future<Output = Result<CrawlJob, DbErr>> + Send;
    fn find_by_key(
        &self,
        key: &str,
    ) -> impl Future<Output = Result<Option<CrawlJob>, DbErr>> + Send;
    fn get(&self, id: i64) -> impl Future<Output = Result<Option<CrawlJob>, DbErr>> + Send;
    fn set_status(
        &self,
        id: i64,
        status: CrawlStatus,
    ) -> impl Future<Output = Result<(), DbErr>> + Send;
    fn add_crawled_page(&self, id: i64) -> impl Future<Output = Result<(), DbErr>> + Send;
    fn fail_unfinished(&self) -> impl Future<Output = Result<u64, DbErr>> + Send;
}

#[derive(Clone)]
pub struct PgJobs {
    db: DatabaseConnection,
}

impl PgJobs {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }
}

fn job_from(row: crawl_job::Model) -> CrawlJob {
    CrawlJob {
        id: row.id,
        base_url: row.base_url,
        status: row.status.into(),
        pages_crawled: stored_page_count(row.id, "pages_crawled", row.pages_crawled),
        pages_skipped: stored_page_count(row.id, "pages_skipped", row.pages_skipped),
    }
}

fn stored_page_count(job_id: i64, column: &str, value: i32) -> u32 {
    u32::try_from(value).unwrap_or_else(|error| {
        tracing::error!(
            job = job_id,
            column,
            value,
            "invalid crawl job counter: {error}"
        );
        0
    })
}

impl JobStore for PgJobs {
    async fn create(
        &self,
        base_url: &str,
        idempotency_key: Option<&str>,
    ) -> Result<CrawlJob, DbErr> {
        let now = Utc::now();
        let row = crawl_job::ActiveModel {
            base_url: Set(base_url.to_owned()),
            status: Set(Status::Queued),
            pages_crawled: Set(0),
            pages_skipped: Set(0),
            idempotency_key: Set(idempotency_key.map(str::to_owned)),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(&self.db)
        .await?;
        Ok(job_from(row))
    }

    async fn find_by_key(&self, key: &str) -> Result<Option<CrawlJob>, DbErr> {
        crawl_job::Entity::find()
            .filter(crawl_job::Column::IdempotencyKey.eq(key))
            .one(&self.db)
            .await
            .map(|row| row.map(job_from))
    }

    async fn get(&self, id: i64) -> Result<Option<CrawlJob>, DbErr> {
        crawl_job::Entity::find_by_id(id)
            .one(&self.db)
            .await
            .map(|row| row.map(job_from))
    }

    async fn set_status(&self, id: i64, status: CrawlStatus) -> Result<(), DbErr> {
        crawl_job::Entity::update_many()
            .col_expr(crawl_job::Column::Status, Expr::value(Status::from(status)))
            .col_expr(crawl_job::Column::UpdatedAt, Expr::value(Utc::now()))
            .filter(crawl_job::Column::Id.eq(id))
            .exec(&self.db)
            .await
            .map(|_| ())
    }

    async fn add_crawled_page(&self, id: i64) -> Result<(), DbErr> {
        crawl_job::Entity::update_many()
            .col_expr(
                crawl_job::Column::PagesCrawled,
                Expr::col(crawl_job::Column::PagesCrawled).add(1),
            )
            .col_expr(crawl_job::Column::UpdatedAt, Expr::value(Utc::now()))
            .filter(crawl_job::Column::Id.eq(id))
            .exec(&self.db)
            .await
            .map(|_| ())
    }

    async fn fail_unfinished(&self) -> Result<u64, DbErr> {
        crawl_job::Entity::update_many()
            .col_expr(crawl_job::Column::Status, Expr::value(Status::Failed))
            .col_expr(crawl_job::Column::UpdatedAt, Expr::value(Utc::now()))
            .filter(crawl_job::Column::Status.is_in([Status::Queued, Status::Running]))
            .exec(&self.db)
            .await
            .map(|updated| updated.rows_affected)
    }
}

#[derive(Clone)]
pub struct Jobs<S, J, K, E> {
    pages: S,
    jobs: J,
    knowledge_base: K,
    edges: E,
    running: Arc<Semaphore>,
}

impl<S: PageStore, J: JobStore, K: KnowledgeBase, E: EdgeStore> Jobs<S, J, K, E> {
    pub fn new(
        pages: S,
        jobs: J,
        knowledge_base: K,
        edges: E,
        max_concurrent_crawls: usize,
    ) -> Self {
        Self {
            pages,
            jobs,
            knowledge_base,
            edges,
            running: Arc::new(Semaphore::new(max_concurrent_crawls)),
        }
    }

    pub async fn create(
        &self,
        base_url: &str,
        idempotency_key: Option<&str>,
    ) -> Result<(CrawlJob, bool), DbErr> {
        if let Some(key) = idempotency_key
            && let Some(existing) = self.jobs.find_by_key(key).await?
        {
            return Ok((existing, false));
        }
        match self.jobs.create(base_url, idempotency_key).await {
            Ok(job) => Ok((job, true)),
            Err(insert_failed) => match idempotency_key {
                Some(key) => match self.jobs.find_by_key(key).await? {
                    Some(raced_in_first) => Ok((raced_in_first, false)),
                    None => Err(insert_failed),
                },
                None => Err(insert_failed),
            },
        }
    }

    pub async fn get(&self, id: i64) -> Result<Option<CrawlJob>, DbErr> {
        self.jobs.get(id).await
    }

    pub async fn fail_unfinished(&self) -> Result<u64, DbErr> {
        self.jobs.fail_unfinished().await
    }

    pub async fn run(&self, id: i64, base_url: &str, scope: Scope, limits: Limits) {
        let _permit = self
            .running
            .clone()
            .acquire_owned()
            .await
            .expect("the crawl semaphore is never closed");
        self.set_status(id, CrawlStatus::Running).await;

        let (counted, to_store) = mpsc::channel::<CountedPage>(limits.max_pages as usize);
        let (fetched, to_graph) = mpsc::channel::<FetchedPage>(limits.max_pages as usize);
        let crawling = async move {
            crawl::crawl(
                base_url,
                &scope,
                limits,
                move |page| {
                    if let Err(error) = counted.try_send(page) {
                        tracing::warn!(job = id, "dropped a counted page: {error}");
                    }
                },
                move |page| {
                    if let Err(error) = fetched.try_send(page) {
                        tracing::warn!(job = id, "dropped a fetched page for the graph: {error}");
                    }
                },
            )
            .await
        };
        let storing = self.store_counted_pages(id, to_store);
        let updating_graph_edges = self.update_graph_edges(id, to_graph);
        let (outcome, (), ()) = tokio::join!(crawling, storing, updating_graph_edges);

        let status = match outcome {
            Ok(()) => CrawlStatus::Done,
            Err(Unreachable) => CrawlStatus::Failed,
        };
        self.set_status(id, status).await;
    }

    async fn store_counted_pages(&self, id: i64, mut to_store: mpsc::Receiver<CountedPage>) {
        while let Some(page) = to_store.recv().await {
            self.store_counted_page(id, page).await;
        }
    }

    async fn store_counted_page(&self, id: i64, page: CountedPage) {
        let url = page.url.clone();
        let Some(saved) = self
            .pages
            .save(page)
            .await
            .inspect_err(|error| tracing::error!(job = id, "could not store {url}: {error}"))
            .ok()
        else {
            return;
        };
        tracing::debug!(job = id, content_changed = saved.changed, "stored {url}");
        self.count_stored_page(id).await;
        self.ingest_stored_page(id, &url, &saved.title, &saved.main_text)
            .await;
    }

    async fn count_stored_page(&self, id: i64) {
        if let Err(error) = self.jobs.add_crawled_page(id).await {
            tracing::error!(job = id, "could not count a stored page: {error}");
        }
    }

    async fn ingest_stored_page(&self, id: i64, url: &str, title: &str, main_text: &str) {
        if let Err(error) = self.knowledge_base.ingest(url, title, main_text).await {
            tracing::error!(job = id, "could not hand {url} to knowledge-base: {error}");
        }
    }

    async fn update_graph_edges(&self, id: i64, mut to_graph: mpsc::Receiver<FetchedPage>) {
        while let Some(page) = to_graph.recv().await {
            self.update_page_graph(id, page).await;
        }
    }

    async fn update_page_graph(&self, id: i64, page: FetchedPage) {
        let Some(discovered_links) = links::try_extract_links(&page.final_url, &page.html) else {
            tracing::warn!(
                job = id,
                "skipped graph edges for {}: extraction did not run",
                page.final_url
            );
            return;
        };
        if let Err(error) = self
            .edges
            .replace_outbound(&page.final_url, discovered_links)
            .await
        {
            tracing::error!(
                job = id,
                "could not update graph edges for {}: {error}",
                page.final_url
            );
        }
    }

    async fn set_status(&self, id: i64, status: CrawlStatus) {
        if let Err(error) = self.jobs.set_status(id, status).await {
            tracing::error!(job = id, "could not mark the job {status:?}: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::time::Duration;

    use common::proto::crawler::v1::CrawlScope;
    use sea_orm::EntityTrait;

    use super::*;
    use crate::crawl::CountedPage;
    use crate::entity::page;
    use crate::store::{PageStore, PgPages, Saved};
    use crate::test_db;
    use crate::test_site::{TestSite, unreachable_url};

    const LIMITS: Limits = Limits {
        max_pages: 50,
        request_timeout: Duration::from_secs(5),
    };
    const ONE_AT_A_TIME: usize = 1;

    #[derive(Clone, Default)]
    struct MemoryPages(Arc<Mutex<Vec<String>>>);

    impl PageStore for MemoryPages {
        async fn save(&self, page: CountedPage) -> Result<Saved, DbErr> {
            self.0.lock().unwrap().push(page.url);
            Ok(Saved {
                title: String::new(),
                main_text: String::new(),
                changed: true,
            })
        }
    }

    #[derive(Clone)]
    struct UnchangedPages;

    impl PageStore for UnchangedPages {
        async fn save(&self, _page: CountedPage) -> Result<Saved, DbErr> {
            Ok(Saved {
                title: "Title".to_owned(),
                main_text: "Content".to_owned(),
                changed: false,
            })
        }
    }

    #[derive(Clone)]
    struct FailingPages;

    impl PageStore for FailingPages {
        async fn save(&self, _page: CountedPage) -> Result<Saved, DbErr> {
            Err(DbErr::Custom("database is down".into()))
        }
    }

    #[derive(Clone, Default)]
    struct NoopKnowledgeBase;

    impl KnowledgeBase for NoopKnowledgeBase {
        async fn ingest(&self, _url: &str, _title: &str, _content: &str) -> Result<(), String> {
            Ok(())
        }
    }

    #[derive(Clone, Default)]
    struct RecordingKnowledgeBase {
        ingested: Arc<Mutex<Vec<String>>>,
        fails: bool,
    }

    impl RecordingKnowledgeBase {
        fn failing() -> Self {
            Self {
                fails: true,
                ..Self::default()
            }
        }
    }

    impl KnowledgeBase for RecordingKnowledgeBase {
        async fn ingest(&self, url: &str, _title: &str, _content: &str) -> Result<(), String> {
            if self.fails {
                return Err("knowledge-base is unavailable".to_owned());
            }
            self.ingested.lock().unwrap().push(url.to_owned());
            Ok(())
        }
    }

    type KeyedJob = (CrawlJob, Option<String>);

    #[derive(Clone, Default)]
    struct MemoryJobs {
        rows: Arc<Mutex<HashMap<i64, KeyedJob>>>,
    }

    impl JobStore for MemoryJobs {
        async fn create(&self, base_url: &str, key: Option<&str>) -> Result<CrawlJob, DbErr> {
            let mut rows = self.rows.lock().unwrap();
            if key.is_some() && rows.values().any(|(_, k)| k.as_deref() == key) {
                return Err(DbErr::Custom("duplicate idempotency key".into()));
            }
            let job = CrawlJob {
                id: rows.len() as i64 + 1,
                base_url: base_url.to_owned(),
                status: CrawlStatus::Queued,
                pages_crawled: 0,
                pages_skipped: 0,
            };
            rows.insert(job.id, (job.clone(), key.map(str::to_owned)));
            Ok(job)
        }

        async fn find_by_key(&self, key: &str) -> Result<Option<CrawlJob>, DbErr> {
            Ok(self
                .rows
                .lock()
                .unwrap()
                .values()
                .find(|(_, k)| k.as_deref() == Some(key))
                .map(|(job, _)| job.clone()))
        }

        async fn get(&self, id: i64) -> Result<Option<CrawlJob>, DbErr> {
            Ok(self
                .rows
                .lock()
                .unwrap()
                .get(&id)
                .map(|(job, _)| job.clone()))
        }

        async fn set_status(&self, id: i64, status: CrawlStatus) -> Result<(), DbErr> {
            if let Some((job, _)) = self.rows.lock().unwrap().get_mut(&id) {
                job.status = status;
            }
            Ok(())
        }

        async fn add_crawled_page(&self, id: i64) -> Result<(), DbErr> {
            if let Some((job, _)) = self.rows.lock().unwrap().get_mut(&id) {
                job.pages_crawled += 1;
            }
            Ok(())
        }

        async fn fail_unfinished(&self) -> Result<u64, DbErr> {
            Ok(0)
        }
    }

    #[derive(Clone, Default)]
    struct NoopEdges;

    impl EdgeStore for NoopEdges {
        async fn replace_outbound(
            &self,
            _from_url: &str,
            _links: Vec<crate::links::Link>,
        ) -> Result<(), DbErr> {
            Ok(())
        }

        async fn neighbors(
            &self,
            _start_url: &str,
            _relation_types: Vec<crate::entity::page_edge::RelationType>,
            _max_depth: u32,
        ) -> Result<Vec<crate::graph::Neighbor>, DbErr> {
            Ok(Vec::new())
        }
    }

    type ReplacedEdges = Vec<(String, Vec<crate::links::Link>)>;

    #[derive(Clone, Default)]
    struct RecordingEdges {
        replaced: Arc<Mutex<ReplacedEdges>>,
    }

    impl EdgeStore for RecordingEdges {
        async fn replace_outbound(
            &self,
            from_url: &str,
            links: Vec<crate::links::Link>,
        ) -> Result<(), DbErr> {
            self.replaced
                .lock()
                .unwrap()
                .push((from_url.to_owned(), links));
            Ok(())
        }

        async fn neighbors(
            &self,
            _start_url: &str,
            _relation_types: Vec<crate::entity::page_edge::RelationType>,
            _max_depth: u32,
        ) -> Result<Vec<crate::graph::Neighbor>, DbErr> {
            Ok(Vec::new())
        }
    }

    #[derive(Clone, Default)]
    struct FailingEdges;

    impl EdgeStore for FailingEdges {
        async fn replace_outbound(
            &self,
            _from_url: &str,
            _links: Vec<crate::links::Link>,
        ) -> Result<(), DbErr> {
            Err(DbErr::Custom("graph store is down".into()))
        }

        async fn neighbors(
            &self,
            _start_url: &str,
            _relation_types: Vec<crate::entity::page_edge::RelationType>,
            _max_depth: u32,
        ) -> Result<Vec<crate::graph::Neighbor>, DbErr> {
            Ok(Vec::new())
        }
    }

    fn in_memory(
        pages: impl PageStore,
    ) -> Jobs<impl PageStore, MemoryJobs, NoopKnowledgeBase, NoopEdges> {
        Jobs::new(
            pages,
            MemoryJobs::default(),
            NoopKnowledgeBase,
            NoopEdges,
            ONE_AT_A_TIME,
        )
    }

    fn in_memory_with_edges(
        pages: impl PageStore,
        edges: impl EdgeStore,
    ) -> Jobs<impl PageStore, MemoryJobs, NoopKnowledgeBase, impl EdgeStore> {
        Jobs::new(
            pages,
            MemoryJobs::default(),
            NoopKnowledgeBase,
            edges,
            ONE_AT_A_TIME,
        )
    }

    fn in_memory_with(
        pages: impl PageStore,
        knowledge_base: impl KnowledgeBase,
    ) -> Jobs<impl PageStore, MemoryJobs, impl KnowledgeBase, NoopEdges> {
        Jobs::new(
            pages,
            MemoryJobs::default(),
            knowledge_base,
            NoopEdges,
            ONE_AT_A_TIME,
        )
    }

    async fn job(
        jobs: &Jobs<impl PageStore, impl JobStore, impl KnowledgeBase, impl EdgeStore>,
        id: i64,
    ) -> CrawlJob {
        jobs.get(id).await.unwrap().unwrap()
    }

    #[tokio::test]
    async fn new_jobs_are_queued_under_distinct_ids() {
        let jobs = in_memory(MemoryPages::default());

        let (first, _) = jobs.create("https://a.example", None).await.unwrap();
        let (second, _) = jobs.create("https://b.example", None).await.unwrap();

        assert_ne!(first.id, second.id);
        assert_eq!(second.public_id(), format!("job-{}", second.id));
        let found = job(&jobs, second.id).await;
        assert_eq!(found.status, CrawlStatus::Queued);
        assert_eq!(found.base_url, "https://b.example");
        assert_eq!(found.pages_crawled, 0);
    }

    #[tokio::test]
    async fn the_same_idempotency_key_returns_the_first_job_instead_of_a_second_crawl() {
        let jobs = in_memory(MemoryPages::default());

        let (first, created) = jobs.create("https://a.example", Some("k1")).await.unwrap();
        let (again, created_again) = jobs.create("https://a.example", Some("k1")).await.unwrap();

        assert!(created);
        assert!(!created_again);
        assert_eq!(again, first);
    }

    #[test]
    fn public_ids_round_trip_and_garbage_does_not_parse() {
        assert_eq!(parse_job_id("job-42"), Some(42));
        for garbage in ["42", "job-", "job-x", "", "job-1-2"] {
            assert_eq!(parse_job_id(garbage), None, "{garbage}");
        }
    }

    #[test]
    fn negative_stored_page_counts_are_reported_as_zero_without_panicking() {
        let now = Utc::now();
        let job = job_from(crawl_job::Model {
            id: 42,
            base_url: "https://example.com".to_owned(),
            status: Status::Running,
            pages_crawled: -1,
            pages_skipped: -2,
            idempotency_key: None,
            created_at: now,
            updated_at: now,
        });

        assert_eq!(job.pages_crawled, 0);
        assert_eq!(job.pages_skipped, 0);
    }

    #[tokio::test]
    async fn unknown_job_is_absent() {
        assert!(
            in_memory(MemoryPages::default())
                .get(404)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn finished_job_is_done_with_its_in_scope_page_count() {
        let site = TestSite::start([
            ("/", "/docs/a /docs/b /blog/x"),
            ("/docs/a", ""),
            ("/docs/b", ""),
            ("/blog/x", ""),
        ])
        .await;
        let jobs = in_memory(MemoryPages::default());
        let (created, _) = jobs.create(&site.url("/"), None).await.unwrap();
        let scope = Scope::new(&CrawlScope {
            include_patterns: vec!["/docs/*".into()],
            ..Default::default()
        });

        jobs.run(created.id, &site.url("/"), scope, LIMITS).await;

        let finished = job(&jobs, created.id).await;
        assert_eq!(finished.status, CrawlStatus::Done);
        assert_eq!(finished.pages_crawled, 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unreachable_site_fails_the_job() {
        let url = unreachable_url().await;
        let jobs = in_memory(MemoryPages::default());
        let (created, _) = jobs.create(&url, None).await.unwrap();

        jobs.run(created.id, &url, Scope::new(&CrawlScope::default()), LIMITS)
            .await;

        let finished = job(&jobs, created.id).await;
        assert_eq!(finished.status, CrawlStatus::Failed);
        assert_eq!(finished.pages_crawled, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn job_is_running_with_a_partial_count_while_the_crawl_is_in_progress() {
        let slow_enough_to_see_the_job_mid_crawl = Duration::from_millis(200);
        let site = TestSite::start_with_response_delay(
            [
                ("/", "/p/1 /p/2 /p/3 /p/4 /p/5"),
                ("/p/1", ""),
                ("/p/2", ""),
                ("/p/3", ""),
                ("/p/4", ""),
                ("/p/5", ""),
            ],
            slow_enough_to_see_the_job_mid_crawl,
        )
        .await;
        let jobs = in_memory(MemoryPages::default());
        let (created, _) = jobs.create(&site.url("/"), None).await.unwrap();

        let crawl = tokio::spawn({
            let (jobs, id, url) = (jobs.clone(), created.id, site.url("/"));
            async move {
                jobs.run(id, &url, Scope::new(&CrawlScope::default()), LIMITS)
                    .await
            }
        });

        let mut seen_mid_crawl = false;
        while !crawl.is_finished() {
            let current = job(&jobs, created.id).await;
            if current.status == CrawlStatus::Running && (1..6).contains(&current.pages_crawled) {
                seen_mid_crawl = true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        crawl.await.unwrap();

        assert!(
            seen_mid_crawl,
            "never saw RUNNING with a partial page count"
        );
        let finished = job(&jobs, created.id).await;
        assert_eq!(finished.status, CrawlStatus::Done);
        assert_eq!(finished.pages_crawled, 6);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_second_crawl_waits_in_the_queue_while_the_cap_is_taken() {
        let slow = Duration::from_millis(200);
        let site = TestSite::start_with_response_delay([("/", "/p/1"), ("/p/1", "")], slow).await;
        let jobs = in_memory(MemoryPages::default());
        let (first, _) = jobs.create(&site.url("/"), None).await.unwrap();
        let (second, _) = jobs.create(&site.url("/"), None).await.unwrap();

        let both = tokio::spawn({
            let (jobs, url) = (jobs.clone(), site.url("/"));
            async move {
                tokio::join!(
                    jobs.run(first.id, &url, Scope::new(&CrawlScope::default()), LIMITS),
                    jobs.run(second.id, &url, Scope::new(&CrawlScope::default()), LIMITS),
                )
            }
        });

        let mut saw_one_running_one_queued = false;
        while !both.is_finished() {
            let statuses = (
                job(&jobs, first.id).await.status,
                job(&jobs, second.id).await.status,
            );
            if matches!(
                statuses,
                (CrawlStatus::Running, CrawlStatus::Queued)
                    | (CrawlStatus::Queued, CrawlStatus::Running)
            ) {
                saw_one_running_one_queued = true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        both.await.unwrap();

        assert!(saw_one_running_one_queued, "both crawls ran at once");
        assert_eq!(job(&jobs, first.id).await.status, CrawlStatus::Done);
        assert_eq!(job(&jobs, second.id).await.status, CrawlStatus::Done);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn job_counts_exactly_the_pages_the_store_accepted() {
        let site = TestSite::start([
            ("/", "/docs/a /docs/b /blog/x"),
            ("/docs/a", ""),
            ("/docs/b", ""),
            ("/blog/x", ""),
        ])
        .await;
        let store = MemoryPages::default();
        let jobs = in_memory(store.clone());
        let (created, _) = jobs.create(&site.url("/"), None).await.unwrap();
        let scope = Scope::new(&CrawlScope {
            include_patterns: vec!["/docs/*".into()],
            ..Default::default()
        });

        jobs.run(created.id, &site.url("/"), scope, LIMITS).await;

        let mut saved = store.0.lock().unwrap().clone();
        saved.sort();
        assert_eq!(saved, [site.url("/docs/a"), site.url("/docs/b")]);
        assert_eq!(job(&jobs, created.id).await.pages_crawled, 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pages_that_fail_to_store_are_not_counted_and_the_job_still_finishes() {
        let site = TestSite::start([("/", "/docs/a"), ("/docs/a", "")]).await;
        let jobs = in_memory(FailingPages);
        let (created, _) = jobs.create(&site.url("/"), None).await.unwrap();

        jobs.run(
            created.id,
            &site.url("/"),
            Scope::new(&CrawlScope::default()),
            LIMITS,
        )
        .await;

        let finished = job(&jobs, created.id).await;
        assert_eq!(finished.status, CrawlStatus::Done);
        assert_eq!(finished.pages_crawled, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_crawl_extracts_and_replaces_outbound_links_for_every_fetched_page() {
        let site = TestSite::start([("/", "/docs/a"), ("/docs/a", "")]).await;
        let edges = RecordingEdges::default();
        let jobs = in_memory_with_edges(MemoryPages::default(), edges.clone());
        let (created, _) = jobs.create(&site.url("/"), None).await.unwrap();

        jobs.run(
            created.id,
            &site.url("/"),
            Scope::new(&CrawlScope::default()),
            LIMITS,
        )
        .await;

        let replaced = edges.replaced.lock().unwrap();
        let root = replaced
            .iter()
            .find(|(from_url, _)| *from_url == site.url("/"))
            .unwrap_or_else(|| panic!("no edges recorded from {}: {replaced:?}", site.url("/")));
        assert_eq!(
            root.1,
            [crate::links::Link {
                url: site.url("/docs/a"),
                anchor_text: "/docs/a".to_owned(),
            }]
        );
        let child = replaced
            .iter()
            .find(|(from_url, _)| *from_url == site.url("/docs/a"))
            .unwrap_or_else(|| {
                panic!(
                    "no edges recorded from {}: {replaced:?}",
                    site.url("/docs/a")
                )
            });
        assert_eq!(
            child.1,
            [],
            "a page with no links should replace with an empty set"
        );
    }

    #[tokio::test]
    async fn a_final_url_at_the_byte_limit_reaches_the_graph_store() {
        let edges = RecordingEdges::default();
        let jobs = in_memory_with_edges(MemoryPages::default(), edges.clone());
        let url = format!("https://example.com/{}", "a".repeat(1024 - 20));
        assert_eq!(url.len(), 1024);
        let (sender, receiver) = mpsc::channel(1);
        sender
            .send(crate::crawl::FetchedPage {
                final_url: url.clone(),
                html: String::new(),
            })
            .await
            .unwrap();
        drop(sender);

        jobs.update_graph_edges(0, receiver).await;

        assert_eq!(edges.replaced.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn html_over_the_parseable_size_never_replaces_existing_edges() {
        let edges = RecordingEdges::default();
        let jobs = in_memory_with_edges(MemoryPages::default(), edges.clone());
        let oversized_html = "x".repeat(crate::links::MAX_PARSEABLE_HTML_BYTES + 1);
        let (sender, receiver) = mpsc::channel(1);
        sender
            .send(crate::crawl::FetchedPage {
                final_url: "https://example.com/a".to_owned(),
                html: oversized_html,
            })
            .await
            .unwrap();
        drop(sender);

        jobs.update_graph_edges(0, receiver).await;

        assert!(
            edges.replaced.lock().unwrap().is_empty(),
            "skipped extraction must never call replace_outbound"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn html_over_the_parseable_size_leaves_a_real_stored_edge_intact() {
        let test = test_db::start().await;
        let real_edges = crate::graph::PgEdges::new(test.db.clone());
        real_edges
            .replace_outbound(
                "https://example.com/a",
                vec![crate::links::Link {
                    url: "https://example.com/b".to_owned(),
                    anchor_text: "B".to_owned(),
                }],
            )
            .await
            .unwrap();
        let jobs = in_memory_with_edges(MemoryPages::default(), real_edges.clone());
        let oversized_html = "x".repeat(crate::links::MAX_PARSEABLE_HTML_BYTES + 1);
        let (sender, receiver) = mpsc::channel(1);
        sender
            .send(crate::crawl::FetchedPage {
                final_url: "https://example.com/a".to_owned(),
                html: oversized_html,
            })
            .await
            .unwrap();
        drop(sender);

        jobs.update_graph_edges(0, receiver).await;

        let neighbors = real_edges
            .neighbors(
                "https://example.com/a",
                vec![],
                crate::graph::MAX_REQUESTABLE_DEPTH,
            )
            .await
            .unwrap();
        assert_eq!(
            neighbors.iter().map(|n| n.url.as_str()).collect::<Vec<_>>(),
            ["https://example.com/b"],
            "the previously stored edge must survive a skipped, oversized extraction"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failing_graph_store_does_not_fail_the_job_or_skip_knowledge_base() {
        let site = TestSite::start([("/", "/docs/a"), ("/docs/a", "")]).await;
        let knowledge_base = RecordingKnowledgeBase::default();
        let jobs = Jobs::new(
            MemoryPages::default(),
            MemoryJobs::default(),
            knowledge_base.clone(),
            FailingEdges,
            ONE_AT_A_TIME,
        );
        let (created, _) = jobs.create(&site.url("/"), None).await.unwrap();

        jobs.run(created.id, &site.url("/"), only_docs(), LIMITS)
            .await;

        let finished = job(&jobs, created.id).await;
        assert_eq!(finished.status, CrawlStatus::Done);
        assert_eq!(finished.pages_crawled, 1);
        assert_eq!(
            knowledge_base.ingested.lock().unwrap().as_slice(),
            [site.url("/docs/a")]
        );
    }

    fn only_docs() -> Scope {
        Scope::new(&CrawlScope {
            include_patterns: vec!["/docs/*".into()],
            ..Default::default()
        })
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_changed_page_is_handed_to_knowledge_base() {
        let site = TestSite::start([("/", "/docs/a"), ("/docs/a", "")]).await;
        let knowledge_base = RecordingKnowledgeBase::default();
        let jobs = in_memory_with(MemoryPages::default(), knowledge_base.clone());
        let (created, _) = jobs.create(&site.url("/"), None).await.unwrap();

        jobs.run(created.id, &site.url("/"), only_docs(), LIMITS)
            .await;

        assert_eq!(
            knowledge_base.ingested.lock().unwrap().as_slice(),
            [site.url("/docs/a")]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_unchanged_page_is_still_handed_to_knowledge_base() {
        let knowledge_base = RecordingKnowledgeBase::default();
        let jobs = in_memory_with(UnchangedPages, knowledge_base.clone());
        let (sender, receiver) = mpsc::channel(1);
        sender
            .send(CountedPage {
                url: "https://example.com/docs/a".to_owned(),
                html: String::new(),
                status: 200,
            })
            .await
            .unwrap();
        drop(sender);

        jobs.store_counted_pages(1, receiver).await;

        assert_eq!(
            knowledge_base.ingested.lock().unwrap().as_slice(),
            ["https://example.com/docs/a"]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_hand_off_to_knowledge_base_does_not_fail_the_job() {
        let site = TestSite::start([("/", "/docs/a"), ("/docs/a", "")]).await;
        let jobs = in_memory_with(MemoryPages::default(), RecordingKnowledgeBase::failing());
        let (created, _) = jobs.create(&site.url("/"), None).await.unwrap();

        jobs.run(created.id, &site.url("/"), only_docs(), LIMITS)
            .await;

        let finished = job(&jobs, created.id).await;
        assert_eq!(finished.status, CrawlStatus::Done);
        assert_eq!(finished.pages_crawled, 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn crawl_stores_the_in_scope_pages_and_the_job_row_in_postgres() {
        let test = test_db::start().await;
        let site =
            TestSite::start([("/", "/docs/a /blog/x"), ("/docs/a", ""), ("/blog/x", "")]).await;
        let jobs = Jobs::new(
            PgPages::new(test.db.clone()),
            PgJobs::new(test.db.clone()),
            NoopKnowledgeBase,
            crate::graph::PgEdges::new(test.db.clone()),
            ONE_AT_A_TIME,
        );
        let (created, _) = jobs.create(&site.url("/"), Some("k1")).await.unwrap();
        let scope = Scope::new(&CrawlScope {
            include_patterns: vec!["/docs/*".into()],
            ..Default::default()
        });

        jobs.run(created.id, &site.url("/"), scope, LIMITS).await;

        let urls: Vec<String> = page::Entity::find()
            .all(&test.db)
            .await
            .unwrap()
            .into_iter()
            .map(|row| row.url)
            .collect();
        assert_eq!(urls, [site.url("/docs/a")]);
        let finished = job(&jobs, created.id).await;
        assert_eq!(finished.status, CrawlStatus::Done);
        assert_eq!(finished.pages_crawled, 1);
        let (same, created_again) = jobs.create(&site.url("/"), Some("k1")).await.unwrap();
        assert!(!created_again);
        assert_eq!(same.id, created.id);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn jobs_left_unfinished_by_a_restart_are_failed_on_startup() {
        let test = test_db::start().await;
        let jobs = Jobs::new(
            PgPages::new(test.db.clone()),
            PgJobs::new(test.db.clone()),
            NoopKnowledgeBase,
            crate::graph::PgEdges::new(test.db.clone()),
            ONE_AT_A_TIME,
        );
        let (queued, _) = jobs.create("https://a.example", None).await.unwrap();
        let (running, _) = jobs.create("https://b.example", None).await.unwrap();
        jobs.set_status(running.id, CrawlStatus::Running).await;
        let (done, _) = jobs.create("https://c.example", None).await.unwrap();
        jobs.set_status(done.id, CrawlStatus::Done).await;

        assert_eq!(jobs.fail_unfinished().await.unwrap(), 2);

        assert_eq!(job(&jobs, queued.id).await.status, CrawlStatus::Failed);
        assert_eq!(job(&jobs, running.id).await.status, CrawlStatus::Failed);
        assert_eq!(job(&jobs, done.id).await.status, CrawlStatus::Done);
    }
}
