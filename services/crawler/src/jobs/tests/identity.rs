// Verifies crawl-job identity, idempotency, and stored model conversion.

use super::*;

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
