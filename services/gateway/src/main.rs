// External entry point. Serves the browser bundle Frontend built, forwards the same Connect contract to internal services over gRPC, and gates every page and RPC behind a session.

mod auth;
mod entity;
mod sessions;
#[cfg(test)]
mod test_db;

use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Redirect, Response as HttpResponse};
use axum::routing::{get, get_service, post};
use common::cache::CacheStore;
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
    ConnectError, ErrorCode, Protocol, RequestContext, Response, Router as ConnectRouter,
    ServiceRequest, ServiceResult,
};
use sea_orm::Database;
use tower_http::services::ServeFile;
use tower_http::set_header::SetResponseHeaderLayer;

use crate::auth::{COOKIE_NAME, cookie_value, generate_token, hash_token, passwords_match};
use crate::sessions::PgCacheStore;

const CRAWLER_CALL_TIMEOUT: Duration = Duration::from_secs(10);
const CRAWLER_CALL_ATTEMPTS: usize = 3;
const CRAWLER_RETRY_DELAY: Duration = Duration::from_millis(100);
const KNOWLEDGE_BASE_CALL_TIMEOUT: Duration = Duration::from_secs(10);
// Every page behind require_session is gated per-session; a client-side cache would let a browser
// show a stale authenticated page after logout without ever asking the server again.
const PAGE_CACHE_HEADER: &str = "no-store";
const DEFAULT_SESSION_TTL_HOURS: u64 = 24;
const SESSION_SWEEP_INTERVAL: Duration = Duration::from_secs(3600);
const UNAUTHENTICATED_RPC_BODY: &str = r#"{"code":"unauthenticated","message":"log in first"}"#;
const LOGIN_REJECTED_BODY: &str = "Wrong password. <a href=\"/login\">Try again</a>";

struct Gateway {
    crawler: CrawlerServiceClient<HttpClient>,
    knowledge_base: KnowledgeBaseServiceClient<HttpClient>,
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
        request: ServiceRequest<'_, IngestRequest>,
    ) -> ServiceResult<IngestResponse> {
        let upstream = self
            .knowledge_base
            .ingest(request.to_owned_message())
            .await?
            .into_owned();

        Response::ok(upstream)
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

#[derive(Clone)]
struct Auth {
    sessions: PgCacheStore,
    password: String,
    session_ttl: Duration,
}

async fn require_session(State(auth): State<Auth>, request: Request, next: Next) -> HttpResponse {
    let token = request
        .headers()
        .get(header::COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(|header| cookie_value(header, COOKIE_NAME));

    let Some(token) = token else {
        return unauthenticated(request.uri().path());
    };

    match auth.sessions.get(&hash_token(token)).await {
        Ok(Some(_)) => next.run(request).await,
        Ok(None) => unauthenticated(request.uri().path()),
        Err(error) => {
            tracing::error!("session lookup failed: {error:?}");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "the session store is not available",
            )
                .into_response()
        }
    }
}

fn unauthenticated(path: &str) -> HttpResponse {
    if matches!(path, "/" | "/sources") {
        Redirect::to("/login").into_response()
    } else {
        (
            StatusCode::UNAUTHORIZED,
            [(header::CONTENT_TYPE, "application/json")],
            UNAUTHENTICATED_RPC_BODY,
        )
            .into_response()
    }
}

#[derive(serde::Deserialize)]
struct LoginForm {
    password: String,
}

async fn login_submit(
    State(auth): State<Auth>,
    axum::Form(form): axum::Form<LoginForm>,
) -> HttpResponse {
    if !passwords_match(&form.password, &auth.password) {
        return (
            StatusCode::UNAUTHORIZED,
            axum::response::Html(LOGIN_REJECTED_BODY),
        )
            .into_response();
    }

    let token = generate_token();
    if let Err(error) = auth
        .sessions
        .set(&hash_token(&token), b"", auth.session_ttl)
        .await
    {
        tracing::error!("could not create a session: {error:?}");
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "could not log in; try again",
        )
            .into_response();
    }

    let cookie = format!(
        "{COOKIE_NAME}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}",
        auth.session_ttl.as_secs()
    );
    (
        StatusCode::SEE_OTHER,
        [
            (header::SET_COOKIE, cookie),
            (header::LOCATION, "/".to_owned()),
        ],
    )
        .into_response()
}

async fn logout(State(auth): State<Auth>, headers: HeaderMap) -> HttpResponse {
    let token = headers
        .get(header::COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(|header| cookie_value(header, COOKIE_NAME));
    if let Some(token) = token
        && let Err(error) = auth.sessions.delete(&hash_token(token)).await
    {
        tracing::error!("could not delete a session on logout: {error:?}");
    }

    let cleared = format!("{COOKIE_NAME}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0");
    (
        StatusCode::SEE_OTHER,
        [
            (header::SET_COOKIE, cleared),
            (header::LOCATION, "/login".to_owned()),
        ],
    )
        .into_response()
}

async fn sweep_expired_sessions_periodically(sessions: PgCacheStore) {
    let mut interval = tokio::time::interval(SESSION_SWEEP_INTERVAL);
    loop {
        interval.tick().await;
        match sessions.drop_expired(chrono::Utc::now()).await {
            Ok(removed) if removed > 0 => tracing::info!("swept {removed} expired sessions"),
            Ok(_) => {}
            Err(error) => tracing::error!("could not sweep expired sessions: {error}"),
        }
    }
}

fn env(name: &str) -> Result<String, String> {
    std::env::var(name).map_err(|_| format!("{name} is not set"))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    common::logging::init();
    let crawler_url =
        std::env::var("CRAWLER_URL").unwrap_or_else(|_| "http://127.0.0.1:8081".into());
    let knowledge_base_url =
        std::env::var("KNOWLEDGE_BASE_URL").unwrap_or_else(|_| "http://127.0.0.1:8084".into());
    let frontend_dir = std::env::var("FRONTEND_DIST_DIR").unwrap_or_else(|_| "/frontend".into());
    let session_ttl_hours = match std::env::var("GATEWAY_SESSION_TTL_HOURS") {
        Ok(value) => value
            .parse::<u64>()
            .map_err(|error| format!("GATEWAY_SESSION_TTL_HOURS: {error}"))?,
        Err(_) => DEFAULT_SESSION_TTL_HOURS,
    };

    let database_url = env("DATABASE_URL")?;
    let db = Database::connect(&database_url).await?;
    db.get_schema_registry("gateway::entity::*")
        .sync(&db)
        .await?;
    let sessions = PgCacheStore::new(db);
    tokio::spawn(sweep_expired_sessions_periodically(sessions.clone()));

    let auth = Auth {
        sessions,
        password: env("GATEWAY_AUTH_PASSWORD")?,
        session_ttl: Duration::from_secs(session_ttl_hours * 3600),
    };

    let gateway = Gateway {
        crawler: CrawlerServiceClient::new(
            HttpClient::plaintext_http2_only(),
            ClientConfig::new(crawler_url.parse()?)
                .with_protocol(Protocol::Grpc)
                .with_default_timeout(CRAWLER_CALL_TIMEOUT)
                .proto(),
        ),
        knowledge_base: KnowledgeBaseServiceClient::new(
            HttpClient::plaintext_http2_only(),
            ClientConfig::new(knowledge_base_url.parse()?)
                .with_protocol(Protocol::Grpc)
                .with_default_timeout(KNOWLEDGE_BASE_CALL_TIMEOUT)
                .proto(),
        ),
    };

    let gateway = Arc::new(gateway);
    let connect = ConnectRouter::new()
        .add_service::<_, common::proto::crawler::v1::CrawlerServiceRegisterMarker>(Arc::clone(
            &gateway,
        ))
        .add_service::<_, common::proto::knowledge_base::v1::KnowledgeBaseServiceRegisterMarker>(
            gateway,
        );

    let protected = axum::Router::new()
        .route_service("/", ServeFile::new(format!("{frontend_dir}/index.html")))
        .route_service(
            "/sources",
            ServeFile::new(format!("{frontend_dir}/sources.html")),
        )
        .layer(SetResponseHeaderLayer::if_not_present(
            header::CACHE_CONTROL,
            HeaderValue::from_static(PAGE_CACHE_HEADER),
        ))
        .fallback_service(connect.into_axum_service())
        .layer(middleware::from_fn_with_state(
            auth.clone(),
            require_session,
        ));

    let public = axum::Router::new()
        .route("/health", get(|| async { "OK" }))
        .route(
            "/login",
            get_service(ServeFile::new(format!("{frontend_dir}/login.html"))).post(login_submit),
        )
        .route("/logout", post(logout))
        .with_state(auth);

    let app = public.merge(protected);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await?;
    tracing::info!(
        "gateway listening on 0.0.0.0:8080 -> crawler at {crawler_url}, knowledge-base at {knowledge_base_url}"
    );
    axum::serve(listener, app).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use buffa::EnumValue;
    use common::proto::crawler::v1::{
        GetCrawlJobResponse, PageNeighbor, PageRelation, StartCrawlResponse,
    };
    use common::proto::knowledge_base::v1::SearchResult;

    use super::*;

    struct FakeKnowledgeBase {
        ingested: std::sync::Mutex<Vec<IngestRequest>>,
        searched: std::sync::Mutex<Vec<SearchRequest>>,
        failure: Option<ErrorCode>,
    }

    #[allow(refining_impl_trait)]
    impl KnowledgeBaseService for FakeKnowledgeBase {
        async fn ingest(
            &self,
            _ctx: RequestContext,
            request: ServiceRequest<'_, IngestRequest>,
        ) -> ServiceResult<IngestResponse> {
            self.ingested
                .lock()
                .unwrap()
                .push(request.to_owned_message());
            if let Some(code) = self.failure {
                return Err(ConnectError::new(code, "configured knowledge-base failure"));
            }
            Response::ok(IngestResponse {
                stored: true,
                skipped: false,
                ..Default::default()
            })
        }

        async fn search(
            &self,
            _ctx: RequestContext,
            request: ServiceRequest<'_, SearchRequest>,
        ) -> ServiceResult<SearchResponse> {
            self.searched
                .lock()
                .unwrap()
                .push(request.to_owned_message());
            if let Some(code) = self.failure {
                return Err(ConnectError::new(code, "configured knowledge-base failure"));
            }
            Response::ok(SearchResponse {
                results: vec![SearchResult {
                    source: "crawler".to_owned(),
                    source_id: "https://example.com/a".to_owned(),
                    title: "Title".to_owned(),
                    summary: "Summary".to_owned(),
                    page_type: "documentation".to_owned(),
                    score: 0.75,
                    keywords: vec!["docs".to_owned()],
                    snippet: "Matching passage".to_owned(),
                    updated_at: "2026-09-13T00:00:00Z".to_owned(),
                    ..Default::default()
                }],
                ..Default::default()
            })
        }
    }

    fn fake_knowledge_base(failure: Option<ErrorCode>) -> Arc<FakeKnowledgeBase> {
        Arc::new(FakeKnowledgeBase {
            ingested: std::sync::Mutex::new(Vec::new()),
            searched: std::sync::Mutex::new(Vec::new()),
            failure,
        })
    }

    async fn start_fake_knowledge_base(service: Arc<FakeKnowledgeBase>) -> String {
        let connect = ConnectRouter::new().add_service(service);
        let app = axum::Router::new().fallback_service(connect.into_axum_service());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{address}")
    }

    struct FakeCrawler {
        received: std::sync::Mutex<Option<GetPageNeighborsRequest>>,
        neighbor_attempts: AtomicUsize,
        neighbor_failures_remaining: AtomicUsize,
        neighbor_failure_code: ErrorCode,
        start_crawl_attempts: AtomicUsize,
        start_crawl_failure_code: Option<ErrorCode>,
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

    fn fake_crawler(failures: usize, failure_code: ErrorCode) -> Arc<FakeCrawler> {
        Arc::new(FakeCrawler {
            received: std::sync::Mutex::new(None),
            neighbor_attempts: AtomicUsize::new(0),
            neighbor_failures_remaining: AtomicUsize::new(failures),
            neighbor_failure_code: failure_code,
            start_crawl_attempts: AtomicUsize::new(0),
            start_crawl_failure_code: None,
        })
    }

    async fn start_fake_crawler(service: Arc<FakeCrawler>) -> String {
        let connect = ConnectRouter::new().add_service(service);
        let app = axum::Router::new().fallback_service(connect.into_axum_service());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{address}")
    }

    async fn start_gateway(gateway: Gateway) -> String {
        let gateway = Arc::new(gateway);
        let connect = ConnectRouter::new()
            .add_service::<_, common::proto::crawler::v1::CrawlerServiceRegisterMarker>(
                Arc::clone(&gateway),
            )
            .add_service::<_, common::proto::knowledge_base::v1::KnowledgeBaseServiceRegisterMarker>(
                gateway,
            );
        let app = axum::Router::new().fallback_service(connect.into_axum_service());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{address}")
    }

    fn client_to(url: &str) -> CrawlerServiceClient<HttpClient> {
        CrawlerServiceClient::new(
            HttpClient::plaintext_http2_only(),
            ClientConfig::new(url.parse().unwrap())
                .with_protocol(Protocol::Grpc)
                .with_default_timeout(CRAWLER_CALL_TIMEOUT)
                .proto(),
        )
    }

    fn unreachable_knowledge_base_client() -> KnowledgeBaseServiceClient<HttpClient> {
        KnowledgeBaseServiceClient::new(
            HttpClient::plaintext_http2_only(),
            ClientConfig::new("http://127.0.0.1:1".parse().unwrap())
                .with_protocol(Protocol::Grpc)
                .with_default_timeout(KNOWLEDGE_BASE_CALL_TIMEOUT)
                .proto(),
        )
    }

    fn knowledge_base_client_to(url: &str) -> KnowledgeBaseServiceClient<HttpClient> {
        KnowledgeBaseServiceClient::new(
            HttpClient::plaintext_http2_only(),
            ClientConfig::new(url.parse().unwrap())
                .with_protocol(Protocol::Grpc)
                .with_default_timeout(KNOWLEDGE_BASE_CALL_TIMEOUT)
                .proto(),
        )
    }

    #[tokio::test]
    async fn knowledge_base_ingest_and_search_round_trip_complete_messages() {
        let fake = fake_knowledge_base(None);
        let knowledge_base_url = start_fake_knowledge_base(Arc::clone(&fake)).await;
        let gateway = Gateway {
            crawler: client_to("http://127.0.0.1:1"),
            knowledge_base: knowledge_base_client_to(&knowledge_base_url),
        };
        let gateway_url = start_gateway(gateway).await;
        let client = knowledge_base_client_to(&gateway_url);
        let ingest = IngestRequest {
            source: "manual".to_owned(),
            source_id: "source-1".to_owned(),
            title: "Complete title".to_owned(),
            content: "Complete content".to_owned(),
            ..Default::default()
        };
        let search = SearchRequest {
            query: "complete query".to_owned(),
            page_types: vec!["documentation".to_owned()],
            limit: 7,
            ..Default::default()
        };

        let ingest_response = client.ingest(ingest.clone()).await.unwrap().into_owned();
        let search_response = client.search(search.clone()).await.unwrap().into_owned();

        assert_eq!(*fake.ingested.lock().unwrap(), [ingest]);
        assert_eq!(*fake.searched.lock().unwrap(), [search]);
        assert!(ingest_response.stored);
        assert_eq!(search_response.results.len(), 1);
        assert_eq!(search_response.results[0].snippet, "Matching passage");
        assert_eq!(search_response.results[0].score, 0.75);
    }

    #[tokio::test]
    async fn knowledge_base_upstream_error_propagates_through_gateway() {
        let fake = fake_knowledge_base(Some(ErrorCode::InvalidArgument));
        let knowledge_base_url = start_fake_knowledge_base(fake).await;
        let gateway = Gateway {
            crawler: client_to("http://127.0.0.1:1"),
            knowledge_base: knowledge_base_client_to(&knowledge_base_url),
        };
        let gateway_url = start_gateway(gateway).await;
        let client = knowledge_base_client_to(&gateway_url);

        let ingest_error = client.ingest(IngestRequest::default()).await.unwrap_err();
        let search_error = client.search(SearchRequest::default()).await.unwrap_err();

        assert_eq!(ingest_error.code, ErrorCode::InvalidArgument);
        assert_eq!(search_error.code, ErrorCode::InvalidArgument);
    }

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
        let client = client_to(&gateway_url);

        let error = client
            .start_crawl(StartCrawlRequest::default())
            .await
            .unwrap_err();

        assert_eq!(error.code, ErrorCode::Unavailable);
        assert_eq!(fake_crawler.start_crawl_attempts.load(Ordering::Relaxed), 1);
    }
}
