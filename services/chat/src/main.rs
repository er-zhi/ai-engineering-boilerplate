// chat: session/topic orchestration over Engine. Connects to Postgres, syncs its schema,
// recovers any topics left Running from a prior process, serves Connect RPC.

use std::sync::Arc;

use axum::routing::get;
use chat::entity::topic::Status;
use chat::error::ChatError;
use chat::topic_manager::TopicManager;
use common::proto::chat::v1::{
    ChatEvent, ChatService, CreateTopicRequest, CreateTopicResponse, GetSessionRequest,
    GetSessionResponse, SendTurnRequest, SendTurnResponse, SetFocusRequest, SetFocusResponse,
    StreamEventsRequest, Topic as TopicProto,
};
use connectrpc::{
    ConnectError, RequestContext, Response, Router as ConnectRouter, ServiceRequest, ServiceResult,
    ServiceStream,
};
use futures::StreamExt;
use sea_orm::Database;
use uuid::Uuid;

const CHAT_PORT: &str = "0.0.0.0:8088";

fn env(name: &str) -> Result<String, String> {
    std::env::var(name).map_err(|_| format!("{name} is not set"))
}

fn status_to_str(status: Status) -> &'static str {
    match status {
        Status::Queued => "queued",
        Status::Running => "running",
        Status::Completed => "completed",
        Status::Failed => "failed",
        Status::Cancelled => "cancelled",
    }
}

fn require_principal(ctx: &RequestContext) -> Result<Uuid, ConnectError> {
    common::principal::from_metadata(ctx.headers())
        .map(|p| p.user_id)
        .ok_or_else(|| ConnectError::invalid_argument("request needs a Principal in the metadata"))
}

struct ChatServiceImpl {
    topics: Arc<TopicManager>,
}

#[allow(refining_impl_trait)]
impl ChatService for ChatServiceImpl {
    async fn create_topic(
        &self,
        ctx: RequestContext,
        request: ServiceRequest<'_, CreateTopicRequest>,
    ) -> ServiceResult<CreateTopicResponse> {
        let user_id = require_principal(&ctx)?;
        let msg = request.to_owned_message();
        let parent_id = msg
            .parent_id
            .map(|id| id.parse::<i64>())
            .transpose()
            .map_err(|_| ConnectError::invalid_argument("parent_id is not a valid id"))?;
        let (topic_id, status) = self
            .topics
            .create_topic(user_id, parent_id, msg.title, msg.input_json)
            .await?;
        Response::ok(CreateTopicResponse {
            topic_id: topic_id.to_string(),
            status: status_to_str(status).to_owned(),
            ..Default::default()
        })
    }

    async fn set_focus(
        &self,
        ctx: RequestContext,
        request: ServiceRequest<'_, SetFocusRequest>,
    ) -> ServiceResult<SetFocusResponse> {
        let user_id = require_principal(&ctx)?;
        let msg = request.to_owned_message();
        let topic_id: i64 = msg
            .topic_id
            .parse()
            .map_err(|_| ConnectError::invalid_argument("topic_id is not a valid id"))?;
        self.topics.set_focus(user_id, topic_id).await?;
        Response::ok(SetFocusResponse::default())
    }

    async fn send_turn(
        &self,
        ctx: RequestContext,
        request: ServiceRequest<'_, SendTurnRequest>,
    ) -> ServiceResult<SendTurnResponse> {
        let user_id = require_principal(&ctx)?;
        let msg = request.to_owned_message();
        let turn_id = Uuid::parse_str(&msg.turn_id)
            .map_err(|_| ConnectError::invalid_argument("turn_id is not a valid uuid"))?;
        let topic_id = self.topics.send_turn(user_id, turn_id, msg.content).await?;
        Response::ok(SendTurnResponse {
            topic_id: topic_id.to_string(),
            ..Default::default()
        })
    }

    async fn get_session(
        &self,
        ctx: RequestContext,
        _request: ServiceRequest<'_, GetSessionRequest>,
    ) -> ServiceResult<GetSessionResponse> {
        let user_id = require_principal(&ctx)?;
        let (focus_topic_id, topics) = self
            .topics
            .session
            .get_session_view(user_id)
            .await
            .map_err(ChatError::from)?;
        Response::ok(GetSessionResponse {
            focus_topic_id: focus_topic_id.map(|id| id.to_string()),
            topics: topics
                .into_iter()
                .map(|t| TopicProto {
                    id: t.id.to_string(),
                    parent_id: t.parent_id.map(|id| id.to_string()),
                    title: t.title,
                    status: status_to_str(t.status).to_owned(),
                    execution_id: t.execution_id.map(|id| id.to_string()),
                    result_summary: t.result_summary,
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        })
    }

    async fn stream_events(
        &self,
        ctx: RequestContext,
        _request: ServiceRequest<'_, StreamEventsRequest>,
    ) -> ServiceResult<ServiceStream<ChatEvent>> {
        let user_id = require_principal(&ctx)?;
        let (_, topics) = self
            .topics
            .session
            .get_session_view(user_id)
            .await
            .map_err(ChatError::from)?;
        let snapshot = topics.into_iter().map(|t| {
            Ok(ChatEvent {
                topic_id: t.id.to_string(),
                kind: "topic_created".to_owned(),
                payload_json: serde_json::json!({
                    "title": t.title,
                    "status": status_to_str(t.status),
                    "parent_id": t.parent_id,
                })
                .to_string(),
                occurred_at: t.updated_at.to_rfc3339(),
                ..Default::default()
            })
        });
        let live = tokio_stream::wrappers::BroadcastStream::new(self.topics.events.subscribe())
            .filter_map(|item| async move { item.ok().map(Ok) });
        Response::stream_ok(futures::stream::iter(snapshot).chain(live))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    common::logging::init();

    let database_url = env("DATABASE_URL")?;
    let db = Database::connect(&database_url).await?;
    db.get_schema_registry("chat::entity::*").sync(&db).await?;

    let topics = Arc::new(TopicManager::new(db, &env("ENGINE_URL")?)?);
    topics.recover().await?;

    let chat_service = ChatServiceImpl {
        topics: Arc::clone(&topics),
    };
    let connect = ConnectRouter::new().add_service(Arc::new(chat_service));
    let app = axum::Router::new()
        .route("/health", get(|| async { "OK" }))
        .fallback_service(connect.into_axum_service());

    let listener = tokio::net::TcpListener::bind(CHAT_PORT).await?;
    tracing::info!("chat listening on {CHAT_PORT}");
    axum::serve(listener, app).await?;
    Ok(())
}
