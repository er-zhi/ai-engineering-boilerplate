// Verifies crawl jobs against the real Postgres-backed stores.

use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn crawl_stores_the_in_scope_pages_and_the_job_row_in_postgres() {
    let test = test_db::start().await;
    let site = TestSite::start([("/", "/docs/a /blog/x"), ("/docs/a", ""), ("/blog/x", "")]).await;
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
