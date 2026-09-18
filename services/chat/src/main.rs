// Serves chat.v1.ChatService: prepares the schema, recovers running topics, validates every entry.

use std::sync::Arc;

use axum::routing::get;
use buffa::EnumValue;
use chat::entity::message::MAX_CONTENT_CHARS;
use chat::events::TopicEvent;
use chat::events::TopicEventKind;
use chat::intent::MAX_TITLE_CHARS;
use chat::topic_manager::TopicManager;
use chat::topic_status::status_proto;
use chrono::Utc;
use common::execution_input::ExecutionInput;
use common::proto::chat::v1::{
    ChatEvent, ChatEventKind, ChatService, CreateTopicRequest, CreateTopicResponse,
    GetSessionRequest, GetSessionResponse, Message as MessageProto, ResetSessionRequest,
    ResetSessionResponse, SendTurnRequest, SendTurnResponse, SetFocusRequest, SetFocusResponse,
    StreamEventsRequest, Topic as TopicProto,
};
use connectrpc::{
    ConnectError, RequestContext, Response, Router as ConnectRouter, ServiceRequest, ServiceResult,
    ServiceStream,
};
use futures::StreamExt;
use sea_orm::{ConnectionTrait, Database};
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
use uuid::Uuid;

const CHAT_PORT: &str = "0.0.0.0:8088";
const MAX_INPUT_JSON_BYTES: usize = 16 * 1024;
const PARTITION_MAINTENANCE_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(24 * 60 * 60);
const EVENTS_RETENTION_MONTHS_VAR: &str = "CHAT_EVENTS_RETENTION_MONTHS";

fn env(name: &str) -> Result<String, String> {
    std::env::var(name).map_err(|_| format!("{name} is not set"))
}

fn require_principal(ctx: &RequestContext) -> Result<Uuid, ConnectError> {
    common::principal::from_metadata(ctx.headers())
        .map(|p| p.user_id)
        .ok_or_else(|| ConnectError::invalid_argument("request needs a Principal in the metadata"))
}

fn bounded_title(title: String) -> Result<String, ConnectError> {
    if title.chars().count() > MAX_TITLE_CHARS {
        return Err(ConnectError::invalid_argument(format!(
            "title is longer than {MAX_TITLE_CHARS} characters"
        )));
    }
    Ok(title)
}

fn bounded_content(content: String) -> Result<String, ConnectError> {
    if content.chars().count() > MAX_CONTENT_CHARS {
        return Err(ConnectError::invalid_argument(format!(
            "content is longer than {MAX_CONTENT_CHARS} characters"
        )));
    }
    Ok(content)
}

fn bounded_input_json(input_json: &str) -> Result<serde_json::Value, ConnectError> {
    if input_json.len() > MAX_INPUT_JSON_BYTES {
        return Err(ConnectError::invalid_argument(format!(
            "input_json is larger than {MAX_INPUT_JSON_BYTES} bytes"
        )));
    }
    if input_json.trim().is_empty() {
        return Ok(serde_json::json!({}));
    }
    let parsed: serde_json::Value = serde_json::from_str(input_json)
        .map_err(|_| ConnectError::invalid_argument("input_json is not valid JSON"))?;
    if !parsed.is_object() {
        return Err(ConnectError::invalid_argument(
            "input_json must be a JSON object",
        ));
    }
    Ok(parsed)
}

fn question_of(topic: &chat::entity::topic::Model) -> String {
    let question = ExecutionInput::in_state(&topic.input_json).question;
    if question.is_empty() {
        return topic.title.clone();
    }
    question
}

fn event_kind_to_proto(kind: TopicEventKind) -> ChatEventKind {
    match kind {
        TopicEventKind::TopicCreated => ChatEventKind::TopicCreated,
        TopicEventKind::TopicQueued => ChatEventKind::TopicQueued,
        TopicEventKind::TopicStarted => ChatEventKind::TopicStarted,
        TopicEventKind::TopicProgress => ChatEventKind::TopicProgress,
        TopicEventKind::TopicCompleted => ChatEventKind::TopicCompleted,
        TopicEventKind::TopicFailed => ChatEventKind::TopicFailed,
        TopicEventKind::TopicCancelled => ChatEventKind::TopicCancelled,
        TopicEventKind::FocusChanged => ChatEventKind::FocusChanged,
        TopicEventKind::Notification => ChatEventKind::Notification,
        TopicEventKind::SessionReset => ChatEventKind::SessionReset,
        TopicEventKind::ClarificationNeeded => ChatEventKind::ClarificationNeeded,
    }
}

fn chat_event(event: TopicEvent) -> ChatEvent {
    ChatEvent {
        topic_id: event.topic_id.map(|id| id.to_string()).unwrap_or_default(),
        kind: EnumValue::Known(event_kind_to_proto(event.kind)),
        payload_json: event.payload.to_string(),
        occurred_at: event.occurred_at.to_rfc3339(),
        session_id: event.session_id.to_string(),
        ..Default::default()
    }
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
        let title = bounded_title(msg.title)?;
        let input_json = bounded_input_json(&msg.input_json)?;
        let (topic_id, status) = self
            .topics
            .create_topic(user_id, parent_id, title, input_json)
            .await?;
        Response::ok(CreateTopicResponse {
            topic_id: topic_id.to_string(),
            status: EnumValue::Known(status_proto(status)),
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
        let content = bounded_content(msg.content)?;
        let topic_ids = self.topics.send_turn(user_id, turn_id, content).await?;
        Response::ok(SendTurnResponse {
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
                    status: EnumValue::Known(status_proto(t.status)),
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
        let session = self.topics.session_of_user(user_id).await?;
        let (live, replay) = self.topics.subscribe_and_replay(&session).await?;
        let replay = replay.into_iter().map(|event| Ok(chat_event(event)));
        let this_session = session.id;
        let live =
            tokio_stream::wrappers::BroadcastStream::new(live).filter_map(move |item| async move {
                match item {
                    Ok(event) if event.session_id == this_session => Some(Ok(chat_event(event))),
                    Ok(_) => None,
                    Err(BroadcastStreamRecvError::Lagged(skipped)) => {
                        tracing::warn!(
                            session_id = %this_session,
                            skipped,
                            "chat event subscriber fell behind; its transcript is now incomplete \
                             until it reloads"
                        );
                        None
                    }
                }
            });
        Response::stream_ok(futures::stream::iter(replay).chain(live))
    }
}

fn events_retention_months() -> Option<u32> {
    let value = std::env::var(EVENTS_RETENTION_MONTHS_VAR).ok()?;
    if value.trim().is_empty() {
        return None;
    }
    match value.parse::<u32>() {
        Ok(months) if months > 0 => Some(months),
        _ => {
            tracing::warn!(
                value,
                "{EVENTS_RETENTION_MONTHS_VAR} is not a positive number of months, keeping every \
                 partition"
            );
            None
        }
    }
}

fn spawn_partition_maintenance(db: sea_orm::DatabaseConnection) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(PARTITION_MAINTENANCE_INTERVAL);
        ticker.tick().await;
        loop {
            ticker.tick().await;
            if let Err(error) =
                chat::event_log::maintain(&db, Utc::now(), events_retention_months()).await
            {
                tracing::error!(%error, "chat.events partition maintenance failed");
            }
        }
    });
}

async fn prepare_schema(
    db: &sea_orm::DatabaseConnection,
) -> Result<(), Box<dyn std::error::Error>> {
    db.get_schema_registry("chat::entity::*").sync(db).await?;
    for statement in chat::entity::topic::INDEX_STATEMENTS_CREATED_AFTER_SCHEMA_SYNC.iter() {
        db.execute_unprepared(statement).await?;
    }
    chat::event_log::setup(db).await?;
    if !chat::event_log::is_partitioned(db).await? {
        tracing::error!(
            "chat.events exists as a plain table, so the partitioned CREATE TABLE was a no-op: \
             the event log will grow without a DROP PARTITION retention path. Drop the table (its \
             contents are a replayable log, not working state) and restart."
        );
    }
    chat::event_log::maintain(db, Utc::now(), events_retention_months()).await?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    common::logging::init();

    let database_url = env("DATABASE_URL")?;
    let db = Database::connect(&database_url).await?;
    prepare_schema(&db).await?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_over_long_field_is_refused_at_the_entry_with_the_limit_named() {
        let title = bounded_title("x".repeat(MAX_TITLE_CHARS + 1))
            .expect_err("an over-long title must be refused");
        assert_eq!(title.code, connectrpc::ErrorCode::InvalidArgument);
        assert!(
            title
                .message
                .unwrap_or_default()
                .contains(&MAX_TITLE_CHARS.to_string())
        );

        let content = bounded_content("x".repeat(MAX_CONTENT_CHARS + 1))
            .expect_err("an over-long turn must be refused");
        assert_eq!(content.code, connectrpc::ErrorCode::InvalidArgument);

        let input = bounded_input_json(&"x".repeat(MAX_INPUT_JSON_BYTES + 1))
            .expect_err("an over-large input_json must be refused");
        assert_eq!(input.code, connectrpc::ErrorCode::InvalidArgument);
    }

    #[test]
    fn input_json_is_normalized_into_an_object_or_refused() {
        assert_eq!(
            bounded_input_json("").expect("empty"),
            serde_json::json!({})
        );
        assert_eq!(
            bounded_input_json(r#"{"question":"hi"}"#).expect("object"),
            serde_json::json!({"question": "hi"})
        );
        for rejected in ["[1,2]", "\"text\"", "{not json}"] {
            assert_eq!(
                bounded_input_json(rejected)
                    .expect_err("only a JSON object may reach the database")
                    .code,
                connectrpc::ErrorCode::InvalidArgument,
                "{rejected}"
            );
        }
    }

    #[test]
    fn a_title_at_the_limit_is_accepted() {
        let exact = "x".repeat(MAX_TITLE_CHARS);
        assert_eq!(bounded_title(exact.clone()).expect("at the limit"), exact);
    }
}
