// chat: session/topic orchestration over Engine. Connects to Postgres, syncs its schema,
// recovers any topics left Running from a prior process, serves Connect RPC.

use std::sync::Arc;

use axum::routing::get;
use chat::status_to_str;
use chat::topic_manager::TopicManager;
use chrono::Utc;
use common::proto::chat::v1::{
    ChatEvent, ChatService, CreateTopicRequest, CreateTopicResponse, GetSessionRequest,
    GetSessionResponse, Message as MessageProto, ResetSessionRequest, ResetSessionResponse,
    SendTurnRequest, SendTurnResponse, SetFocusRequest, SetFocusResponse, StreamEventsRequest,
    Topic as TopicProto,
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

fn require_principal(ctx: &RequestContext) -> Result<Uuid, ConnectError> {
    common::principal::from_metadata(ctx.headers())
        .map(|p| p.user_id)
        .ok_or_else(|| ConnectError::invalid_argument("request needs a Principal in the metadata"))
}

/// The self-contained question a topic was started with. The classifier writes it into
/// `input_json.question` (it carries the context a background worker needs, which the short
/// `title` does not); topics created before that, or through `CreateTopic` with some other input
/// shape, fall back to their title.
fn question_of(topic: &chat::entity::topic::Model) -> String {
    serde_json::from_str::<serde_json::Value>(&topic.input_json)
        .ok()
        .as_ref()
        .and_then(|input| input.get("question"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| topic.title.clone())
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
        let topic_ids = self.topics.send_turn(user_id, turn_id, msg.content).await?;
        Response::ok(SendTurnResponse {
            topic_id: topic_ids.first().map(i64::to_string).unwrap_or_default(),
            topic_ids: topic_ids.iter().map(i64::to_string).collect(),
            ..Default::default()
        })
    }

    async fn get_session(
        &self,
        ctx: RequestContext,
        _request: ServiceRequest<'_, GetSessionRequest>,
    ) -> ServiceResult<GetSessionResponse> {
        let user_id = require_principal(&ctx)?;
        let (focus_topic_id, topics) = self.topics.session.get_session_view(user_id).await?;
        let topic_ids: Vec<i64> = topics.iter().map(|t| t.id).collect();
        let mut messages = self.topics.session.messages_by_topic(&topic_ids).await?;
        Response::ok(GetSessionResponse {
            focus_topic_id: focus_topic_id.map(|id| id.to_string()),
            topics: topics
                .into_iter()
                .map(|t| TopicProto {
                    id: t.id.to_string(),
                    parent_id: t.parent_id.map(|id| id.to_string()),
                    question: question_of(&t),
                    status: status_to_str(t.status).to_owned(),
                    execution_id: t.execution_id.map(|id| id.to_string()),
                    result_summary: t.result_summary,
                    created_at: t.created_at.to_rfc3339(),
                    updated_at: t.updated_at.to_rfc3339(),
                    messages: messages
                        .remove(&t.id)
                        .unwrap_or_default()
                        .into_iter()
                        .map(|m| MessageProto {
                            id: m.id.to_string(),
                            content: m.content,
                            created_at: m.created_at.to_rfc3339(),
                            turn_id: m.turn_id.to_string(),
                            ..Default::default()
                        })
                        .collect(),
                    title: t.title,
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        })
    }

    async fn reset_session(
        &self,
        ctx: RequestContext,
        _request: ServiceRequest<'_, ResetSessionRequest>,
    ) -> ServiceResult<ResetSessionResponse> {
        let user_id = require_principal(&ctx)?;
        self.topics.reset_session(user_id).await?;
        Response::ok(ResetSessionResponse::default())
    }

    async fn stream_events(
        &self,
        ctx: RequestContext,
        _request: ServiceRequest<'_, StreamEventsRequest>,
    ) -> ServiceResult<ServiceStream<ChatEvent>> {
        let user_id = require_principal(&ctx)?;
        let session_id = self.topics.session.get_or_create_session(user_id).await?.id;
        // Subscribe before reading the replay, never after — see `subscribe_then_snapshot`.
        // The replay is the session's whole stored history in `chat.events`, not a synthetic
        // `topic_created` per topic: that is what lets a reloaded page rebuild the debug panel
        // (and everything else it saw) instead of starting blank.
        let (live, replay) = chat::events::subscribe_then_snapshot(
            &self.topics.events,
            self.topics.stored_events(session_id),
        )
        .await?;
        let replay = replay.into_iter().map(Ok);
        // The bus is one process-wide broadcast channel, so it carries every user's events; the
        // live tail is filtered down to this session here rather than left to the client.
        let wanted = session_id.to_string();
        let live = tokio_stream::wrappers::BroadcastStream::new(live).filter_map(move |item| {
            let wanted = wanted.clone();
            async move {
                item.ok()
                    .filter(|event: &ChatEvent| event.session_id == wanted)
                    .map(Ok)
            }
        });
        Response::stream_ok(futures::stream::iter(replay).chain(live))
    }
}

/// Keeps `chat.events`' partition window open ahead of `now`. Startup alone is not enough: a
/// process that stays up across a month boundary would hit a range with no partition, and the
/// INSERT itself fails then. Once a day is far more often than needed and costs two `IF NOT
/// EXISTS` statements.
fn spawn_partition_maintenance(db: sea_orm::DatabaseConnection) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(24 * 60 * 60));
        ticker.tick().await; // fires immediately; startup already did this pass
        loop {
            ticker.tick().await;
            if let Err(error) = chat::event_log::ensure_current_and_next(&db, Utc::now()).await {
                tracing::error!(%error, "failed to open the next chat.events partition");
            }
        }
    });
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    common::logging::init();

    let database_url = env("DATABASE_URL")?;
    let db = Database::connect(&database_url).await?;
    db.get_schema_registry("chat::entity::*").sync(&db).await?;
    // `chat.events` is partitioned, so schema-sync cannot own it — see `chat::event_log`.
    chat::event_log::setup(&db).await?;
    spawn_partition_maintenance(db.clone());

    let topics = Arc::new(TopicManager::new(
        db,
        &env("ENGINE_URL")?,
        &env("LLM_ROUTER_URL")?,
    )?);
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
