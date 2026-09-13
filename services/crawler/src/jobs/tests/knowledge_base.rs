// Verifies page handoff to the knowledge-base dependency.

use super::*;

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
