use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use buffa::EnumValue;
use common::proto::crawler::v1::{
    CrawlerService, CrawlerServiceClient, GetCrawlJobRequest, GetCrawlJobResponse,
    GetPageNeighborsRequest, GetPageNeighborsResponse, PageNeighbor, PageRelation,
    StartCrawlRequest, StartCrawlResponse,
};
use common::proto::knowledge_base::v1::KnowledgeBaseServiceClient;
use connectrpc::client::{ClientConfig, HttpClient};
use connectrpc::{
    ConnectError, ErrorCode, Protocol, RequestContext, Response, Router as ConnectRouter,
    ServiceRequest, ServiceResult,
};

use crate::proxy::{CRAWLER_CALL_TIMEOUT, KNOWLEDGE_BASE_CALL_TIMEOUT};

pub(super) struct FakeCrawler {
    pub(super) received: std::sync::Mutex<Option<GetPageNeighborsRequest>>,
    pub(super) neighbor_attempts: AtomicUsize,
    pub(super) neighbor_failures_remaining: AtomicUsize,
    pub(super) neighbor_failure_code: ErrorCode,
    pub(super) start_crawl_attempts: AtomicUsize,
    pub(super) start_crawl_failure_code: Option<ErrorCode>,
}

#[allow(refining_impl_trait)]
impl CrawlerService for FakeCrawler {
    async fn start_crawl(
        &self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, StartCrawlRequest>,
    ) -> ServiceResult<StartCrawlResponse> {
        self.start_crawl_attempts.fetch_add(1, Ordering::Relaxed);
        if let Some(code) = self.start_crawl_failure_code {
            return Err(ConnectError::new(code, "configured fake crawler failure"));
        }
        Response::ok(StartCrawlResponse::default())
    }

    async fn get_crawl_job(
        &self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, GetCrawlJobRequest>,
    ) -> ServiceResult<GetCrawlJobResponse> {
        Response::ok(GetCrawlJobResponse::default())
    }

    async fn get_page_neighbors(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, GetPageNeighborsRequest>,
    ) -> ServiceResult<GetPageNeighborsResponse> {
        self.neighbor_attempts.fetch_add(1, Ordering::Relaxed);
        *self.received.lock().unwrap() = Some(request.to_owned_message());
        if self
            .neighbor_failures_remaining
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
        {
            return Err(ConnectError::new(
                self.neighbor_failure_code,
                "configured fake crawler failure",
            ));
        }
        Response::ok(GetPageNeighborsResponse {
            shallowest_first_neighbors: vec![PageNeighbor {
                crawled: true,
                url: "https://example.com/found".to_owned(),
                title: "Found".to_owned(),
                relation: EnumValue::Known(PageRelation::Canonical),
                depth: 2,
                ..Default::default()
            }],
            ..Default::default()
        })
    }
}

pub(super) fn fake_crawler(failures: usize, failure_code: ErrorCode) -> Arc<FakeCrawler> {
    Arc::new(FakeCrawler {
        received: std::sync::Mutex::new(None),
        neighbor_attempts: AtomicUsize::new(0),
        neighbor_failures_remaining: AtomicUsize::new(failures),
        neighbor_failure_code: failure_code,
        start_crawl_attempts: AtomicUsize::new(0),
        start_crawl_failure_code: None,
    })
}

pub(super) async fn start_fake_crawler(service: Arc<FakeCrawler>) -> String {
    let connect = ConnectRouter::new().add_service(service);
    let app = axum::Router::new().fallback_service(connect.into_axum_service());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{address}")
}

pub(super) fn client_to(url: &str) -> CrawlerServiceClient<HttpClient> {
    CrawlerServiceClient::new(
        HttpClient::plaintext_http2_only(),
        ClientConfig::new(url.parse().unwrap())
            .with_protocol(Protocol::Grpc)
            .with_default_timeout(CRAWLER_CALL_TIMEOUT)
            .proto(),
    )
}

pub(super) fn unreachable_knowledge_base_client() -> KnowledgeBaseServiceClient<HttpClient> {
    KnowledgeBaseServiceClient::new(
        HttpClient::plaintext_http2_only(),
        ClientConfig::new("http://127.0.0.1:1".parse().unwrap())
            .with_protocol(Protocol::Grpc)
            .with_default_timeout(KNOWLEDGE_BASE_CALL_TIMEOUT)
            .proto(),
    )
}
