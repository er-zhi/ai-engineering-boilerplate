use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use buffa::EnumValue;
use common::proto::crawler::v1::{
    CrawlerServiceClient, GetCrawlJobRequest, GetCrawlJobResponse, GetPageNeighborsRequest,
    PageRelation, StartCrawlRequest,
};
use connectrpc::ErrorCode;

use super::start_gateway;
use crate::proxy::Gateway;

#[path = "crawler/support.rs"]
mod support;
use support::*;

#[tokio::test]
async fn get_page_neighbors_forwards_the_complete_request_and_response() {
    let fake_crawler = fake_crawler(0, ErrorCode::Unavailable);
    let crawler_url = start_fake_crawler(Arc::clone(&fake_crawler)).await;
    let gateway = Gateway {
        crawler: client_to(&crawler_url),
        knowledge_base: unreachable_knowledge_base_client(),
    };
    let gateway_url = start_gateway(gateway).await;
    let client = client_to(&gateway_url);
    let sent = GetPageNeighborsRequest {
        url: "https://example.com/start".to_owned(),
        allowed_relation_types: vec![
            EnumValue::Known(PageRelation::LinksTo),
            EnumValue::Known(PageRelation::Canonical),
        ],
        max_depth: 2,
        ..Default::default()
    };
    let response = client
        .get_page_neighbors(sent.clone())
        .await
        .unwrap()
        .into_owned();
    assert_eq!(*fake_crawler.received.lock().unwrap(), Some(sent));
    assert_eq!(response.shallowest_first_neighbors.len(), 1);
    let neighbor = &response.shallowest_first_neighbors[0];
    assert!(neighbor.crawled);
    assert_eq!(neighbor.url, "https://example.com/found");
    assert_eq!(neighbor.title, "Found");
    assert_eq!(neighbor.relation, EnumValue::Known(PageRelation::Canonical));
    assert_eq!(neighbor.depth, 2);
}

#[tokio::test]
async fn get_crawl_job_is_forwarded_through_gateway() {
    let fake_crawler = fake_crawler(0, ErrorCode::Unavailable);
    let crawler_url = start_fake_crawler(fake_crawler).await;
    let gateway = Gateway {
        crawler: client_to(&crawler_url),
        knowledge_base: unreachable_knowledge_base_client(),
    };
    let gateway_url = start_gateway(gateway).await;
    let client = client_to(&gateway_url);
    let response = client
        .get_crawl_job(GetCrawlJobRequest::default())
        .await
        .unwrap()
        .into_owned();
    assert_eq!(response, GetCrawlJobResponse::default());
}

#[tokio::test]
async fn get_page_neighbors_retries_transient_failures_until_success() {
    let fake_crawler = fake_crawler(2, ErrorCode::Unavailable);
    let crawler_url = start_fake_crawler(Arc::clone(&fake_crawler)).await;
    let gateway = Gateway {
        crawler: client_to(&crawler_url),
        knowledge_base: unreachable_knowledge_base_client(),
    };
    let gateway_url = start_gateway(gateway).await;
    let client = client_to(&gateway_url);
    let response = client
        .get_page_neighbors(GetPageNeighborsRequest::default())
        .await
        .unwrap()
        .into_owned();
    assert_eq!(fake_crawler.neighbor_attempts.load(Ordering::Relaxed), 3);
    assert_eq!(response.shallowest_first_neighbors.len(), 1);
}

#[tokio::test]
async fn get_page_neighbors_does_not_retry_permanent_failures() {
    let fake_crawler = fake_crawler(1, ErrorCode::InvalidArgument);
    let crawler_url = start_fake_crawler(Arc::clone(&fake_crawler)).await;
    let gateway = Gateway {
        crawler: client_to(&crawler_url),
        knowledge_base: unreachable_knowledge_base_client(),
    };
    let gateway_url = start_gateway(gateway).await;
    let client = client_to(&gateway_url);
    let error = client
        .get_page_neighbors(GetPageNeighborsRequest::default())
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidArgument);
    assert_eq!(fake_crawler.neighbor_attempts.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn start_crawl_is_never_retried_even_on_a_transient_failure() {
    let fake_crawler = Arc::new(FakeCrawler {
        received: std::sync::Mutex::new(None),
        neighbor_attempts: AtomicUsize::new(0),
        neighbor_failures_remaining: AtomicUsize::new(0),
        neighbor_failure_code: ErrorCode::Unavailable,
        start_crawl_attempts: AtomicUsize::new(0),
        start_crawl_failure_code: Some(ErrorCode::Unavailable),
    });
    let crawler_url = start_fake_crawler(Arc::clone(&fake_crawler)).await;
    let gateway = Gateway {
        crawler: client_to(&crawler_url),
        knowledge_base: unreachable_knowledge_base_client(),
    };
    let gateway_url = start_gateway(gateway).await;
    let client: CrawlerServiceClient<_> = client_to(&gateway_url);
    let error = client
        .start_crawl(StartCrawlRequest::default())
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::Unavailable);
    assert_eq!(fake_crawler.start_crawl_attempts.load(Ordering::Relaxed), 1);
}
