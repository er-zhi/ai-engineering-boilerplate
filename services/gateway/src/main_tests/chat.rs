// Gateway's Chat passthrough: the Principal it stamps, and the deadline it gives the stream.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use buffa::EnumValue;
use common::principal::Principal;
use common::proto::chat::v1::{
    ChatEvent, ChatEventKind, ChatService, CreateTopicRequest, CreateTopicResponse,
    GetSessionRequest, GetSessionResponse, ResetSessionRequest, ResetSessionResponse,
    SendTurnRequest, SendTurnResponse, SetFocusRequest, SetFocusResponse, StreamEventsRequest,
    TopicStatus,
};
use connectrpc::client::CallOptions;
use connectrpc::{
    ConnectError, RequestContext, Response, Router as ConnectRouter, ServiceRequest, ServiceResult,
    ServiceStream,
};

use super::{
    chat_client_to, start_gateway, unreachable_crawler_client, unreachable_knowledge_base_client,
};
use crate::auth::COOKIE_NAME;
use crate::proxy::Gateway;

#[derive(Default)]
struct FakeChat {
    principals_seen: Mutex<Vec<Principal>>,
    stream_delay_before_first_event: Duration,
}

impl FakeChat {
    fn principal(&self, ctx: &RequestContext) -> Result<Principal, ConnectError> {
        let principal = common::principal::from_metadata(ctx.headers())
            .ok_or_else(|| ConnectError::invalid_argument("request needs a Principal"))?;
        self.principals_seen.lock().unwrap().push(principal.clone());
        Ok(principal)
    }
}

#[allow(refining_impl_trait)]
impl ChatService for FakeChat {
    async fn create_topic(
        &self,
        ctx: RequestContext,
        _request: ServiceRequest<'_, CreateTopicRequest>,
    ) -> ServiceResult<CreateTopicResponse> {
        self.principal(&ctx)?;
        Response::ok(CreateTopicResponse {
            topic_id: "1".to_owned(),
            status: EnumValue::Known(TopicStatus::Running),
            ..Default::default()
        })
    }

    async fn set_focus(
        &self,
        ctx: RequestContext,
        _request: ServiceRequest<'_, SetFocusRequest>,
    ) -> ServiceResult<SetFocusResponse> {
        self.principal(&ctx)?;
        Response::ok(SetFocusResponse::default())
    }

    async fn send_turn(
        &self,
        ctx: RequestContext,
        _request: ServiceRequest<'_, SendTurnRequest>,
    ) -> ServiceResult<SendTurnResponse> {
        self.principal(&ctx)?;
        Response::ok(SendTurnResponse {
            topic_ids: vec!["1".to_owned()],
            ..Default::default()
        })
    }

    async fn get_session(
        &self,
        ctx: RequestContext,
        _request: ServiceRequest<'_, GetSessionRequest>,
    ) -> ServiceResult<GetSessionResponse> {
        self.principal(&ctx)?;
        Response::ok(GetSessionResponse::default())
    }

    async fn reset_session(
        &self,
        ctx: RequestContext,
        _request: ServiceRequest<'_, ResetSessionRequest>,
    ) -> ServiceResult<ResetSessionResponse> {
        self.principal(&ctx)?;
        Response::ok(ResetSessionResponse::default())
    }

    async fn stream_events(
        &self,
        ctx: RequestContext,
        _request: ServiceRequest<'_, StreamEventsRequest>,
    ) -> ServiceResult<ServiceStream<ChatEvent>> {
        self.principal(&ctx)?;
        let delay = self.stream_delay_before_first_event;
        Response::stream_ok(futures::stream::once(async move {
            tokio::time::sleep(delay).await;
            Ok(ChatEvent {
                topic_id: "1".to_owned(),
                kind: EnumValue::Known(ChatEventKind::TopicCompleted),
                ..Default::default()
            })
        }))
    }
}

async fn start_fake_chat(service: Arc<FakeChat>) -> String {
    let connect = ConnectRouter::new().add_service(service);
    let app = axum::Router::new().fallback_service(connect.into_axum_service());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{address}")
}

async fn gateway_over(fake: Arc<FakeChat>, chat_timeout: Duration) -> String {
    let chat_url = start_fake_chat(fake).await;
    start_gateway(Gateway {
        crawler: unreachable_crawler_client(),
        knowledge_base: unreachable_knowledge_base_client(),
        chat: chat_client_to(&chat_url, chat_timeout),
    })
    .await
}

fn with_session_cookie(token: &str) -> CallOptions {
    CallOptions::default().with_header(
        axum::http::header::COOKIE,
        format!("{COOKIE_NAME}={token}; other=ignored"),
    )
}

#[tokio::test]
async fn every_chat_rpc_reaches_chat_with_a_principal_built_from_the_session_cookie() {
    let fake = Arc::new(FakeChat::default());
    let gateway_url = gateway_over(Arc::clone(&fake), Duration::from_secs(10)).await;
    let client = chat_client_to(&gateway_url, Duration::from_secs(10));
    let token = "a1b2c3-session-token";

    client
        .create_topic_with_options(CreateTopicRequest::default(), with_session_cookie(token))
        .await
        .expect("CreateTopic must not be rejected for a missing Principal");
    client
        .set_focus_with_options(SetFocusRequest::default(), with_session_cookie(token))
        .await
        .expect("SetFocus");
    client
        .send_turn_with_options(SendTurnRequest::default(), with_session_cookie(token))
        .await
        .expect("SendTurn");
    client
        .get_session_with_options(GetSessionRequest::default(), with_session_cookie(token))
        .await
        .expect("GetSession");
    client
        .reset_session_with_options(ResetSessionRequest::default(), with_session_cookie(token))
        .await
        .expect("ResetSession");
    let mut stream = client
        .stream_events_with_options(StreamEventsRequest::default(), with_session_cookie(token))
        .await
        .expect("StreamEvents");
    stream
        .message::<ChatEvent>()
        .await
        .expect("the stream must open, which means the Principal was accepted");

    let seen = fake.principals_seen.lock().unwrap();
    assert_eq!(seen.len(), 6, "all six RPCs must carry a Principal");
    for principal in seen.iter() {
        assert_eq!(
            principal.user_id.to_string(),
            "00000000-0000-0000-0000-000000000001",
            "the single-tenant constant user id, parsed back out of the header"
        );
        assert_eq!(
            principal.session_id, token,
            "session_id is the caller's own raw session token, not its stored hash"
        );
    }
}

#[tokio::test]
async fn a_chat_rpc_without_a_session_cookie_is_refused_by_gateway() {
    let fake = Arc::new(FakeChat::default());
    let gateway_url = gateway_over(Arc::clone(&fake), Duration::from_secs(10)).await;
    let client = chat_client_to(&gateway_url, Duration::from_secs(10));

    let error = client
        .create_topic(CreateTopicRequest::default())
        .await
        .expect_err("no cookie, no Principal");

    assert_eq!(error.code, connectrpc::ErrorCode::Unauthenticated);
    assert!(fake.principals_seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn the_event_stream_outlives_the_unary_deadline() {
    let unary_timeout = Duration::from_millis(200);
    let fake = Arc::new(FakeChat {
        principals_seen: Mutex::new(Vec::new()),
        stream_delay_before_first_event: Duration::from_secs(1),
    });
    let gateway_url = gateway_over(Arc::clone(&fake), unary_timeout).await;
    let client = chat_client_to(&gateway_url, Duration::from_secs(30));

    let mut stream = client
        .stream_events_with_options(
            StreamEventsRequest::default(),
            with_session_cookie("a1b2c3-session-token"),
        )
        .await
        .expect("stream");
    let event = stream
        .message::<ChatEvent>()
        .await
        .expect("an event arriving after the unary deadline must still be forwarded")
        .expect("one event");

    assert_eq!(
        event.to_owned_message().kind,
        EnumValue::Known(ChatEventKind::TopicCompleted)
    );
}
