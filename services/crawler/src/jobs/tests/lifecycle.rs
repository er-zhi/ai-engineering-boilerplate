// Verifies crawl-job lifecycle, progress, failure, and concurrency behavior.

use super::*;

#[tokio::test]
async fn any_pipeline_loss_fails_the_job_instead_of_reporting_partial_success() {
    let jobs = in_memory(MemoryPages::default());
    let (created, _) = jobs.create("https://example.com", None).await.unwrap();

    jobs.complete_job(created.id, Err(CrawlError::PagesLost(1))).await;

    assert_eq!(job(&jobs, created.id).await.status, CrawlStatus::Failed);
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
    let response_delay = Duration::from_millis(200);
    let site = TestSite::start_with_response_delay(
        [
            ("/", "/p/1 /p/2 /p/3 /p/4 /p/5"),
            ("/p/1", ""),
            ("/p/2", ""),
            ("/p/3", ""),
            ("/p/4", ""),
            ("/p/5", ""),
        ],
        response_delay,
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

    assert!(seen_mid_crawl, "never saw RUNNING with a partial page count");
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
