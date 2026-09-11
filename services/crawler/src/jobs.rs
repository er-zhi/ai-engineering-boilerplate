// In-memory crawl jobs: registers them, runs them, and keeps their status and page counts current.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use common::proto::crawler::v1::CrawlStatus;

use crate::crawl::{self, Limits, Unreachable};
use crate::scope::Scope;

#[derive(Clone, Debug)]
pub struct CrawlJob {
    pub base_url: String,
    pub status: CrawlStatus,
    pub pages_crawled: u32,
    pub pages_skipped: u32,
}

#[derive(Clone, Default)]
pub struct Jobs {
    jobs: Arc<Mutex<HashMap<String, CrawlJob>>>,
    next_id: Arc<AtomicU64>,
}

impl Jobs {
    pub fn create(&self, base_url: &str) -> String {
        let job_id = format!("job-{}", self.next_id.fetch_add(1, Ordering::Relaxed) + 1);
        let job = CrawlJob {
            base_url: base_url.to_owned(),
            status: CrawlStatus::Queued,
            pages_crawled: 0,
            pages_skipped: 0,
        };
        self.jobs.lock().unwrap().insert(job_id.clone(), job);
        job_id
    }

    pub fn get(&self, job_id: &str) -> Option<CrawlJob> {
        self.jobs.lock().unwrap().get(job_id).cloned()
    }

    pub async fn run(&self, job_id: &str, base_url: &str, scope: Scope, limits: Limits) {
        self.update(job_id, |job| job.status = CrawlStatus::Running);

        let outcome = crawl::crawl(base_url, &scope, limits, |_| {
            self.update(job_id, |job| job.pages_crawled += 1)
        })
        .await;

        let status = match outcome {
            Ok(()) => CrawlStatus::Done,
            Err(Unreachable) => CrawlStatus::Failed,
        };
        self.update(job_id, |job| job.status = status);
    }

    fn update(&self, job_id: &str, change: impl FnOnce(&mut CrawlJob)) {
        if let Some(job) = self.jobs.lock().unwrap().get_mut(job_id) {
            change(job);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use common::proto::crawler::v1::CrawlScope;

    use super::*;
    use crate::test_site::{TestSite, unreachable_url};

    const LIMITS: Limits = Limits {
        max_pages: 50,
        request_timeout: Duration::from_secs(5),
    };

    #[test]
    fn new_jobs_are_queued_under_distinct_ids() {
        let jobs = Jobs::default();

        let first = jobs.create("https://a.example");
        let second = jobs.create("https://b.example");

        assert_ne!(first, second);
        let job = jobs.get(&second).unwrap();
        assert_eq!(job.status, CrawlStatus::Queued);
        assert_eq!(job.base_url, "https://b.example");
        assert_eq!(job.pages_crawled, 0);
    }

    #[test]
    fn unknown_job_is_absent() {
        assert!(Jobs::default().get("job-404").is_none());
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
        let jobs = Jobs::default();
        let id = jobs.create(&site.url("/"));
        let scope = Scope::new(&CrawlScope {
            include_patterns: vec!["/docs/*".into()],
            ..Default::default()
        });

        jobs.run(&id, &site.url("/"), scope, LIMITS).await;

        let job = jobs.get(&id).unwrap();
        assert_eq!(job.status, CrawlStatus::Done);
        assert_eq!(job.pages_crawled, 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unreachable_site_fails_the_job() {
        let url = unreachable_url().await;
        let jobs = Jobs::default();
        let id = jobs.create(&url);

        jobs.run(&id, &url, Scope::new(&CrawlScope::default()), LIMITS)
            .await;

        let job = jobs.get(&id).unwrap();
        assert_eq!(job.status, CrawlStatus::Failed);
        assert_eq!(job.pages_crawled, 0);
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
        let jobs = Jobs::default();
        let id = jobs.create(&site.url("/"));

        let crawl = tokio::spawn({
            let (jobs, id, url) = (jobs.clone(), id.clone(), site.url("/"));
            async move {
                jobs.run(&id, &url, Scope::new(&CrawlScope::default()), LIMITS)
                    .await
            }
        });

        let mut seen_mid_crawl = false;
        while !crawl.is_finished() {
            let job = jobs.get(&id).unwrap();
            if job.status == CrawlStatus::Running && (1..6).contains(&job.pages_crawled) {
                seen_mid_crawl = true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        crawl.await.unwrap();

        assert!(
            seen_mid_crawl,
            "never saw RUNNING with a partial page count"
        );
        let job = jobs.get(&id).unwrap();
        assert_eq!(job.status, CrawlStatus::Done);
        assert_eq!(job.pages_crawled, 6);
    }
}
