// External entry point. Proxies page loads to Frontend and forwards the same Connect contract to internal services over gRPC.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode, Uri};
use axum::response::Response as HttpResponse;
use axum::routing::get;
use common::proto::crawler::v1::{
    CrawlerService, CrawlerServiceClient, GetCrawlJobRequest, GetCrawlJobResponse,
    StartCrawlRequest, StartCrawlResponse,
};
use connectrpc::client::{ClientConfig, HttpClient};
use connectrpc::{
    Protocol, RequestContext, Response, Router as ConnectRouter, ServiceRequest, ServiceResult,
};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;

const CRAWLER_CALL_TIMEOUT: Duration = Duration::from_secs(10);
const PAGE_LOAD_TIMEOUT: Duration = Duration::from_secs(5);

struct Gateway {
    crawler: CrawlerServiceClient<HttpClient>,
}

#[derive(Clone)]
struct Frontend {
    http: Client<HttpConnector, Body>,
    base_url: String,
}

fn bad_gateway(error: impl std::fmt::Display) -> (StatusCode, &'static str) {
    tracing::error!("frontend page load failed: {error}");
    (StatusCode::BAD_GATEWAY, "frontend unreachable")
}

async fn page(
    State(frontend): State<Frontend>,
    uri: Uri,
) -> Result<HttpResponse, (StatusCode, &'static str)> {
    let target: Uri = format!("{}{}", frontend.base_url, uri.path())
        .parse()
        .map_err(bad_gateway)?;

    let request = Request::builder()
        .uri(target)
        .body(Body::empty())
        .map_err(bad_gateway)?;

    let response = tokio::time::timeout(PAGE_LOAD_TIMEOUT, frontend.http.request(request))
        .await
        .map_err(bad_gateway)?
        .map_err(bad_gateway)?;

    Ok(response.map(Body::new))
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
        let upstream = self
            .crawler
            .get_crawl_job(request.to_owned_message())
            .await?
            .into_owned();

        Response::ok(upstream)
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    common::logging::init();
    let crawler_url =
        std::env::var("CRAWLER_URL").unwrap_or_else(|_| "http://127.0.0.1:8081".into());
    let frontend_url =
        std::env::var("FRONTEND_URL").unwrap_or_else(|_| "http://127.0.0.1:8082".into());

    let gateway = Gateway {
        crawler: CrawlerServiceClient::new(
            HttpClient::plaintext_http2_only(),
            ClientConfig::new(crawler_url.parse()?)
                .with_protocol(Protocol::Grpc)
                .with_default_timeout(CRAWLER_CALL_TIMEOUT)
                .proto(),
        ),
    };

    let frontend = Frontend {
        http: Client::builder(TokioExecutor::new()).build_http(),
        base_url: frontend_url.trim_end_matches('/').to_owned(),
    };

    let connect = ConnectRouter::new().add_service(Arc::new(gateway));

    let app = axum::Router::new()
        .route("/", get(page))
        .route("/health", get(|| async { "OK" }))
        .with_state(frontend)
        .fallback_service(connect.into_axum_service());

    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await?;
    tracing::info!(
        "gateway listening on 0.0.0.0:8080 -> crawler at {crawler_url}, frontend at {frontend_url}"
    );
    axum::serve(listener, app).await?;

    Ok(())
}
