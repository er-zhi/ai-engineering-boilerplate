// Verifies page persistence and crawl progress remain consistent.

use super::*;

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
