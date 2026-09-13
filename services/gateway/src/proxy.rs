// Forwards Gateway RPCs to their owning internal services.

use std::future::Future;
use std::time::Duration;

use axum::http::Uri;
use common::proto::crawler::v1::{
    CrawlerService, CrawlerServiceClient, GetCrawlJobRequest, GetCrawlJobResponse,
    GetPageNeighborsRequest, GetPageNeighborsResponse, StartCrawlRequest, StartCrawlResponse,
};
use common::proto::knowledge_base::v1::{
    IngestRequest, IngestResponse, KnowledgeBaseService, KnowledgeBaseServiceClient, SearchRequest,
    SearchResponse,
};
use connectrpc::client::{ClientConfig, HttpClient};
use connectrpc::{
    ConnectError, ErrorCode, Protocol, RequestContext, Response, ServiceRequest, ServiceResult,
};

pub(crate) const CRAWLER_CALL_TIMEOUT: Duration = Duration::from_secs(10);
const CRAWLER_CALL_ATTEMPTS: usize = 3;
const CRAWLER_RETRY_DELAY: Duration = Duration::from_millis(100);
pub(crate) const KNOWLEDGE_BASE_CALL_TIMEOUT: Duration = Duration::from_secs(10);

pub struct Gateway {
    pub(crate) crawler: CrawlerServiceClient<HttpClient>,
    pub(crate) knowledge_base: KnowledgeBaseServiceClient<HttpClient>,
}

impl Gateway {
    pub fn new(crawler_url: Uri, knowledge_base_url: Uri) -> Self {
        Self {
            crawler: CrawlerServiceClient::new(
                HttpClient::plaintext_http2_only(),
                ClientConfig::new(crawler_url)
                    .with_protocol(Protocol::Grpc)
                    .with_default_timeout(CRAWLER_CALL_TIMEOUT)
                    .proto(),
            ),
            knowledge_base: KnowledgeBaseServiceClient::new(
                HttpClient::plaintext_http2_only(),
                ClientConfig::new(knowledge_base_url)
                    .with_protocol(Protocol::Grpc)
                    .with_default_timeout(KNOWLEDGE_BASE_CALL_TIMEOUT)
                    .proto(),
            ),
        }
    }
}

async fn retry_crawler_call<T, F, Fut>(mut call: F) -> Result<T, ConnectError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, ConnectError>>,
{
    let mut attempt = 1;
    loop {
        match call().await {
            Ok(response) => return Ok(response),
            Err(error)
                if attempt < CRAWLER_CALL_ATTEMPTS
                    && matches!(
                        error.code,
                        ErrorCode::Unavailable | ErrorCode::DeadlineExceeded
                    ) =>
            {
                attempt += 1;
                tokio::time::sleep(CRAWLER_RETRY_DELAY).await;
            }
            Err(error) => return Err(error),
        }
    }
}

#[allow(refining_impl_trait)]
impl CrawlerService for Gateway {
    async fn start_crawl(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, StartCrawlRequest>,
    ) -> ServiceResult<StartCrawlResponse> {
        let upstream = self
            .crawler
            .start_crawl(request.to_owned_message())
            .await?
            .into_owned();
        Response::ok(upstream)
    }

    async fn get_crawl_job(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, GetCrawlJobRequest>,
    ) -> ServiceResult<GetCrawlJobResponse> {
        let request = request.to_owned_message();
        let upstream = retry_crawler_call(|| self.crawler.get_crawl_job(request.clone()))
            .await?
            .into_owned();
        Response::ok(upstream)
    }

    async fn get_page_neighbors(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, GetPageNeighborsRequest>,
    ) -> ServiceResult<GetPageNeighborsResponse> {
        let request = request.to_owned_message();
        let upstream = retry_crawler_call(|| self.crawler.get_page_neighbors(request.clone()))
            .await?
            .into_owned();
        Response::ok(upstream)
    }
}

#[allow(refining_impl_trait)]
impl KnowledgeBaseService for Gateway {
    async fn ingest(
        &self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, IngestRequest>,
    ) -> ServiceResult<IngestResponse> {
        Err(ConnectError::unimplemented(
            "ingest is a crawler-to-knowledge-base call, not exposed through Gateway",
        ))
    }

    async fn search(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, SearchRequest>,
    ) -> ServiceResult<SearchResponse> {
        let upstream = self
            .knowledge_base
            .search(request.to_owned_message())
            .await?
            .into_owned();
        Response::ok(upstream)
    }
}
