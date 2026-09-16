// Forwards Gateway RPCs to their owning internal services.

use std::future::Future;
use std::time::Duration;

use axum::http::{Uri, header};
use common::principal::{SESSION_ID_HEADER, USER_ID_HEADER};
use common::proto::chat::v1::{
    ChatEvent, ChatService, ChatServiceClient, CreateTopicRequest, CreateTopicResponse,
    GetSessionRequest, GetSessionResponse, SendTurnRequest, SendTurnResponse, SetFocusRequest,
    SetFocusResponse, StreamEventsRequest,
};
use common::proto::crawler::v1::{
    CrawlerService, CrawlerServiceClient, GetCrawlJobRequest, GetCrawlJobResponse,
    GetPageNeighborsRequest, GetPageNeighborsResponse, StartCrawlRequest, StartCrawlResponse,
};
use common::proto::knowledge_base::v1::{
    IngestRequest, IngestResponse, KnowledgeBaseService, KnowledgeBaseServiceClient,
    ReadDocumentRequest, ReadDocumentResponse, SearchRequest, SearchResponse,
};
use connectrpc::client::{CallOptions, ClientConfig, HttpClient};
use connectrpc::{
    ConnectError, ErrorCode, Protocol, RequestContext, Response, ServiceRequest, ServiceResult,
    ServiceStream,
};

use crate::auth::{COOKIE_NAME, cookie_value};

pub(crate) const CRAWLER_CALL_TIMEOUT: Duration = Duration::from_secs(10);
const CRAWLER_CALL_ATTEMPTS: usize = 3;
const CRAWLER_RETRY_DELAY: Duration = Duration::from_millis(100);
pub(crate) const KNOWLEDGE_BASE_CALL_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const CHAT_CALL_TIMEOUT: Duration = Duration::from_secs(10);
/// `StreamEvents` is a long-lived stream, and a client timeout is a *whole-call* deadline that
/// connectrpc re-checks on every frame — the 10s unary bound would cut the browser's event feed
/// after ten seconds, every time. Long and finite: `CallOptions` cannot express "no deadline".
pub(crate) const CHAT_STREAM_CALL_TIMEOUT: Duration = Duration::from_secs(3_600);

/// This deployment has exactly one logical user. Gateway authenticates a single shared password
/// and stores nothing user-identifying on a session (`web.rs`: the session row's value is
/// empty), so Chat's Principal-per-user-id model degenerates to this constant.
const DEFAULT_USER_ID: &str = "00000000-0000-0000-0000-000000000001";

pub struct Gateway {
    pub(crate) crawler: CrawlerServiceClient<HttpClient>,
    pub(crate) knowledge_base: KnowledgeBaseServiceClient<HttpClient>,
    pub(crate) chat: ChatServiceClient<HttpClient>,
}

impl Gateway {
    pub fn new(crawler_url: Uri, knowledge_base_url: Uri, chat_url: Uri) -> Self {
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
            chat: ChatServiceClient::new(
                HttpClient::plaintext_http2_only(),
                ClientConfig::new(chat_url)
                    .with_protocol(Protocol::Grpc)
                    .with_default_timeout(CHAT_CALL_TIMEOUT)
                    .proto(),
            ),
        }
    }
}

/// Chat rejects any request without a Principal in its metadata (`chat`'s `require_principal`),
/// and Gateway is its only caller — so every proxied Chat RPC carries one, built here.
///
/// The user id is the single-tenant constant above; the session id is the caller's own session
/// token, read from the `Cookie` header exactly the way `require_session` reads it. That
/// middleware has already validated this cookie by the time any of these handlers run, so a
/// missing one means the request did not come through Gateway's session flow at all.
fn chat_call_options(ctx: &RequestContext) -> Result<CallOptions, ConnectError> {
    let session_token = ctx
        .headers()
        .get(header::COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(|cookies| cookie_value(cookies, COOKIE_NAME))
        .ok_or_else(|| ConnectError::unauthenticated("no session cookie on the request"))?;
    CallOptions::default()
        .with_header(USER_ID_HEADER, DEFAULT_USER_ID)
        .try_with_header(SESSION_ID_HEADER, session_token)
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

    async fn read_document(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, ReadDocumentRequest>,
    ) -> ServiceResult<ReadDocumentResponse> {
        let upstream = self
            .knowledge_base
            .read_document(request.to_owned_message())
            .await?
            .into_owned();
        Response::ok(upstream)
    }
}

#[allow(refining_impl_trait)]
impl ChatService for Gateway {
    async fn create_topic(
        &self,
        ctx: RequestContext,
        request: ServiceRequest<'_, CreateTopicRequest>,
    ) -> ServiceResult<CreateTopicResponse> {
        let upstream = self
            .chat
            .create_topic_with_options(request.to_owned_message(), chat_call_options(&ctx)?)
            .await?
            .into_owned();
        Response::ok(upstream)
    }

    async fn set_focus(
        &self,
        ctx: RequestContext,
        request: ServiceRequest<'_, SetFocusRequest>,
    ) -> ServiceResult<SetFocusResponse> {
        let upstream = self
            .chat
            .set_focus_with_options(request.to_owned_message(), chat_call_options(&ctx)?)
            .await?
            .into_owned();
        Response::ok(upstream)
    }

    async fn send_turn(
        &self,
        ctx: RequestContext,
        request: ServiceRequest<'_, SendTurnRequest>,
    ) -> ServiceResult<SendTurnResponse> {
        let upstream = self
            .chat
            .send_turn_with_options(request.to_owned_message(), chat_call_options(&ctx)?)
            .await?
            .into_owned();
        Response::ok(upstream)
    }

    async fn get_session(
        &self,
        ctx: RequestContext,
        request: ServiceRequest<'_, GetSessionRequest>,
    ) -> ServiceResult<GetSessionResponse> {
        let upstream = self
            .chat
            .get_session_with_options(request.to_owned_message(), chat_call_options(&ctx)?)
            .await?
            .into_owned();
        Response::ok(upstream)
    }

    async fn stream_events(
        &self,
        ctx: RequestContext,
        request: ServiceRequest<'_, StreamEventsRequest>,
    ) -> ServiceResult<ServiceStream<ChatEvent>> {
        let options = chat_call_options(&ctx)?.with_timeout(CHAT_STREAM_CALL_TIMEOUT);
        let mut stream = self
            .chat
            .stream_events_with_options(request.to_owned_message(), options)
            .await?;
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            loop {
                match stream.message::<ChatEvent>().await {
                    Ok(Some(item)) => {
                        if sender.send(Ok(item.to_owned_message())).is_err() {
                            break; // receiver dropped
                        }
                    }
                    Ok(None) => break, // clean end of stream
                    Err(error) => {
                        let _ = sender.send(Err(error));
                        break;
                    }
                }
            }
        });
        Response::stream_ok(tokio_stream::wrappers::UnboundedReceiverStream::new(
            receiver,
        ))
    }
}
