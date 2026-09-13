// Verifies crawl orchestration updates graph edges without corrupting prior data.

use super::*;

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
            final_url: url,
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
        neighbors.iter().map(|neighbor| neighbor.url.as_str()).collect::<Vec<_>>(),
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
