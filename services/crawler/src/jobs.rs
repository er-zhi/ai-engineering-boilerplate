// Crawl jobs: rows in crawler.crawl_jobs, run under a concurrency cap, with page counts kept current as pages are stored and changed pages handed to knowledge-base.

use std::sync::Arc;

use common::proto::crawler::v1::CrawlStatus;
use sea_orm::DbErr;
use tokio::sync::{Semaphore, mpsc};

use crate::crawl::{self, CountedPage, CrawlError, FetchedPage, Limits};
use crate::graph::EdgeStore;
use crate::knowledge_base::KnowledgeBase;
use crate::links;
use crate::scope::Scope;
use crate::store::PageStore;

const JOB_ID_PREFIX: &str = "job-";
const PIPELINE_BUFFER_PAGES: usize = 1;
pub(crate) const MAX_CONCURRENT_CRAWLS: usize = 2;
const PIPELINE_HALF_CONTAINER_MEMORY_BUDGET_BYTES: usize = 96 * 1024 * 1024;
const MAX_LIVE_HTML_BODIES_PER_CRAWL: usize =
    PIPELINE_BUFFER_PAGES * 2 + crate::crawl::FETCH_CONCURRENCY + 3;

const _: () = assert!(
    MAX_LIVE_HTML_BODIES_PER_CRAWL * crate::links::MAX_PARSEABLE_HTML_BYTES * MAX_CONCURRENT_CRAWLS
        <= PIPELINE_HALF_CONTAINER_MEMORY_BUDGET_BYTES
);

#[derive(Clone, Debug, PartialEq)]
pub struct CrawlJob {
    pub id: i64,
    pub base_url: String,
    pub status: CrawlStatus,
    pub pages_crawled: u32,
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

        let (counted, to_store) = mpsc::channel::<CountedPage>(PIPELINE_BUFFER_PAGES);
        let (fetched, to_graph) = mpsc::channel::<FetchedPage>(PIPELINE_BUFFER_PAGES);
        let crawling =
            async move { crawl::crawl_into(base_url, &scope, limits, counted, fetched).await };
        let storing = self.store_counted_pages(id, to_store);
        let updating_graph_edges = self.update_graph_edges(id, to_graph);
        let (outcome, (), ()) = tokio::join!(crawling, storing, updating_graph_edges);
        self.complete_job(id, outcome).await;
    }

    async fn complete_job(&self, id: i64, outcome: Result<(), CrawlError>) {
        let status = match outcome {
            Ok(()) => CrawlStatus::Done,
            Err(CrawlError::Unreachable) => CrawlStatus::Failed,
            Err(CrawlError::PagesLost(pages)) => {
                tracing::error!(job = id, pages, "crawl lost pages");
                CrawlStatus::Failed
            }
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
    include!("jobs/tests/mod.rs");
}
