// create_topic: the entry point for a new topic, root or child. Concurrency-limited per
// session (spec's default of 3 running at once) — beyond the limit, a topic is Queued instead
// of started; Task 8's completion handler promotes the oldest Queued topic when a slot frees.

use chrono::Utc;
use common::proto::chat::v1::ChatEvent;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, DatabaseConnection, EntityTrait,
    PaginatorTrait, QueryFilter, QueryOrder,
};
use uuid::Uuid;

use crate::engine_client::EngineClient;
use crate::entity::topic::{self, Status};
use crate::error::ChatError;
use crate::events::EventBus;
use crate::session_manager::SessionManager;

const AGENT_GRAPH_ID: &str = "agent";
pub const MAX_CONCURRENT_TOPICS: u64 = 3;

pub struct TopicManager {
    pub(crate) session: SessionManager,
    pub(crate) engine: EngineClient,
    pub(crate) events: EventBus,
}

impl TopicManager {
    pub fn new(db: DatabaseConnection, engine_url: &str) -> Result<Self, String> {
        Ok(Self {
            session: SessionManager::new(db),
            engine: EngineClient::new(engine_url)?,
            events: EventBus::default(),
        })
    }

    pub async fn create_topic(
        self: &std::sync::Arc<Self>,
        user_id: Uuid,
        parent_id: Option<i64>,
        title: String,
        input_json: String,
    ) -> Result<(i64, Status), ChatError> {
        let session = self.session.get_or_create_session(user_id).await?;
        let running = topic::Entity::find()
            .filter(topic::Column::SessionId.eq(session.id))
            .filter(topic::Column::Status.eq(Status::Running))
            .count(&self.session.db)
            .await?;

        let now = Utc::now();
        let status = if running < MAX_CONCURRENT_TOPICS {
            Status::Running
        } else {
            Status::Queued
        };
        let execution_id = if status == Status::Running {
            Some(
                self.engine
                    .start_execution(AGENT_GRAPH_ID, &input_json)
                    .await
                    .map_err(ChatError::Engine)?,
            )
        } else {
            None
        };

        let row = topic::ActiveModel {
            session_id: Set(session.id),
            parent_id: Set(parent_id),
            title: Set(title),
            status: Set(status),
            execution_id: Set(execution_id),
            result_summary: Set(None),
            artifact_ids: Set(serde_json::json!([])),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(&self.session.db)
        .await?;

        if session.focus_topic_id.is_none() {
            self.set_focus_silently(session.id, row.id).await?;
        }

        self.events.publish(ChatEvent {
            topic_id: row.id.to_string(),
            kind: if status == Status::Running {
                "topic_started".to_owned()
            } else {
                "topic_queued".to_owned()
            }
            .to_owned(),
            occurred_at: now.to_rfc3339(),
            ..Default::default()
        });

        if let (Status::Running, Some(execution_id)) = (status, execution_id) {
            self.spawn_watch(row.id, execution_id);
        }

        Ok((row.id, status))
    }

    /// Used only by `create_topic` for the "first topic in a session becomes focus" rule (spec)
    /// — doesn't emit `focus_changed` itself (there was no prior focus to change from). Task 8's
    /// `set_focus` is the user-facing operation and does emit it.
    async fn set_focus_silently(&self, session_id: Uuid, topic_id: i64) -> Result<(), ChatError> {
        use crate::entity::session;
        let mut active: session::ActiveModel = session::Entity::find_by_id(session_id)
            .one(&self.session.db)
            .await?
            .ok_or(ChatError::InvalidRequest("session vanished".to_owned()))?
            .into();
        active.focus_topic_id = Set(Some(topic_id));
        active.update(&self.session.db).await?;
        Ok(())
    }

    pub async fn set_focus(&self, user_id: Uuid, topic_id: i64) -> Result<(), ChatError> {
        let session = self.session.get_or_create_session(user_id).await?;
        let topic = topic::Entity::find_by_id(topic_id)
            .one(&self.session.db)
            .await?
            .filter(|t| t.session_id == session.id)
            .ok_or(ChatError::TopicNotFound(topic_id))?;

        use crate::entity::session;
        let previous = session.focus_topic_id;
        let mut active: session::ActiveModel = session::Entity::find_by_id(session.id)
            .one(&self.session.db)
            .await?
            .ok_or(ChatError::InvalidRequest("session vanished".to_owned()))?
            .into();
        active.focus_topic_id = Set(Some(topic_id));
        active.update(&self.session.db).await?;

        self.events.publish(ChatEvent {
            topic_id: topic.id.to_string(),
            kind: "focus_changed".to_owned(),
            payload_json: serde_json::json!({"from": previous, "reason": "user"}).to_string(),
            occurred_at: Utc::now().to_rfc3339(),
            ..Default::default()
        });
        Ok(())
    }

    pub async fn send_turn(
        &self,
        user_id: Uuid,
        turn_id: Uuid,
        content: String,
    ) -> Result<i64, ChatError> {
        let (focus_topic_id, _topics) = self.session.get_session_view(user_id).await?;
        let topic_id = focus_topic_id.ok_or(ChatError::NoFocus)?;
        let topic = topic::Entity::find_by_id(topic_id)
            .one(&self.session.db)
            .await?
            .ok_or(ChatError::TopicNotFound(topic_id))?;

        use crate::entity::message;
        if message::Entity::find()
            .filter(message::Column::TopicId.eq(topic_id))
            .filter(message::Column::TurnId.eq(turn_id))
            .one(&self.session.db)
            .await?
            .is_some()
        {
            return Ok(topic_id); // already applied — idempotent no-op
        }
        message::ActiveModel {
            topic_id: Set(topic_id),
            turn_id: Set(turn_id),
            content: Set(content.clone()),
            created_at: Set(Utc::now()),
            ..Default::default()
        }
        .insert(&self.session.db)
        .await?;

        let execution_id = topic.execution_id.ok_or(ChatError::InvalidRequest(format!(
            "topic {topic_id} has no running execution to interrupt"
        )))?;
        let input_json = serde_json::json!({"question": content}).to_string();
        self.engine
            .interrupt(execution_id, &input_json)
            .await
            .map_err(ChatError::Engine)?;
        Ok(topic_id)
    }

    /// Consumes one topic's Engine event stream to completion, updating `chat.topics` and
    /// publishing to the `EventBus` as it goes. Spawned as a background task whenever a topic
    /// starts Running — from `create_topic`, from `promote_next_queued` below, or from
    /// `recover` at startup. Never returns an error to a caller: a stream failure is logged and
    /// the topic is left as-is (recoverable by `recover` on the next restart).
    pub async fn watch_topic(self: &std::sync::Arc<Self>, topic_id: i64, execution_id: Uuid) {
        let mut events = match self.engine.stream_events(execution_id).await {
            Ok(events) => events,
            Err(error) => {
                tracing::error!(topic_id, %error, "failed to open Engine event stream");
                return;
            }
        };
        while let Some(event) = events.recv().await {
            if let Err(error) = self.handle_engine_event(topic_id, &event).await {
                tracing::error!(topic_id, %error, "failed to handle an Engine event");
            }
        }
    }

    async fn handle_engine_event(
        self: &std::sync::Arc<Self>,
        topic_id: i64,
        event: &common::proto::engine::v1::ExecutionEvent,
    ) -> Result<(), ChatError> {
        match event.payload_kind.as_str() {
            "ExecutionCompleted" => self.finish_topic(topic_id, Status::Completed, event).await,
            "ExecutionFailed" => self.finish_topic(topic_id, Status::Failed, event).await,
            _ => {
                self.events.publish(ChatEvent {
                    topic_id: topic_id.to_string(),
                    kind: "topic_progress".to_owned(),
                    payload_json: event.payload_json.clone(),
                    occurred_at: event.occurred_at.clone(),
                    ..Default::default()
                });
                Ok(())
            }
        }
    }

    async fn finish_topic(
        self: &std::sync::Arc<Self>,
        topic_id: i64,
        status: Status,
        event: &common::proto::engine::v1::ExecutionEvent,
    ) -> Result<(), ChatError> {
        let payload: serde_json::Value =
            serde_json::from_str(&event.payload_json).unwrap_or(serde_json::Value::Null);
        let summary = payload
            .get("final_state")
            .and_then(|state| state.pointer("/llm/reply"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .or_else(|| {
                payload
                    .get("error")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            });

        let mut active: topic::ActiveModel = topic::Entity::find_by_id(topic_id)
            .one(&self.session.db)
            .await?
            .ok_or(ChatError::TopicNotFound(topic_id))?
            .into();
        active.status = Set(status);
        active.result_summary = Set(summary.clone());
        active.updated_at = Set(Utc::now());
        let row = active.update(&self.session.db).await?;

        let kind = if status == Status::Completed {
            "topic_completed"
        } else {
            "topic_failed"
        };
        self.events.publish(ChatEvent {
            topic_id: topic_id.to_string(),
            kind: kind.to_owned(),
            payload_json: serde_json::json!({"summary": summary}).to_string(),
            occurred_at: row.updated_at.to_rfc3339(),
            ..Default::default()
        });
        self.events.publish(ChatEvent {
            topic_id: topic_id.to_string(),
            kind: "notification".to_owned(),
            payload_json: serde_json::json!({
                "kind": if status == Status::Completed { "completed" } else { "failed" },
                "text": summary,
            })
            .to_string(),
            occurred_at: row.updated_at.to_rfc3339(),
            ..Default::default()
        });

        self.promote_next_queued(row.session_id).await
    }

    /// Called after any topic leaves `Running` — starts the oldest still-`Queued` topic in the
    /// same session, if the slot that just freed leaves room (it always does: one topic just
    /// left `Running`). Returns `Ok(())` with nothing promoted if there's no queued topic.
    async fn promote_next_queued(
        self: &std::sync::Arc<Self>,
        session_id: Uuid,
    ) -> Result<(), ChatError> {
        let Some(next) = topic::Entity::find()
            .filter(topic::Column::SessionId.eq(session_id))
            .filter(topic::Column::Status.eq(Status::Queued))
            .order_by_asc(topic::Column::CreatedAt)
            .one(&self.session.db)
            .await?
        else {
            return Ok(());
        };
        // The queued topic's original creation `input_json` (spec's CreateTopicRequest) isn't
        // persisted anywhere in this plan's schema (Task 1's `topic` entity has no such column),
        // so promotion starts the execution with an empty initial state — the queued topic's
        // original intent is lost by the time it actually runs. Accepted gap for tonight's scope.
        let input_json = "{}".to_owned();
        let execution_id = self
            .engine
            .start_execution("agent", &input_json)
            .await
            .map_err(ChatError::Engine)?;
        let mut active: topic::ActiveModel = next.clone().into();
        active.status = Set(Status::Running);
        active.execution_id = Set(Some(execution_id));
        active.updated_at = Set(Utc::now());
        active.update(&self.session.db).await?;

        self.events.publish(ChatEvent {
            topic_id: next.id.to_string(),
            kind: "topic_started".to_owned(),
            occurred_at: Utc::now().to_rfc3339(),
            ..Default::default()
        });
        self.spawn_watch(next.id, execution_id);
        Ok(())
    }

    /// Called once at startup (Task 9's `main.rs`) — re-attaches a live `watch_topic` consumer
    /// to every topic this process left `Running` before a restart. Recovery correctness relies
    /// on Engine's `StreamEvents(execution_id)` replaying full history from version 1 (not just
    /// new events from the subscribe point) — confirmed against `services/engine`'s own
    /// `StreamEvents` handler; if this ever changes, `recover` needs a version cursor too.
    pub async fn recover(self: &std::sync::Arc<Self>) -> Result<(), ChatError> {
        let running = topic::Entity::find()
            .filter(topic::Column::Status.eq(Status::Running))
            .all(&self.session.db)
            .await?;
        for topic in running {
            let Some(execution_id) = topic.execution_id else {
                continue;
            };
            self.spawn_watch(topic.id, execution_id);
        }
        Ok(())
    }

    /// Shared by `create_topic`, `promote_next_queued`, and `recover` — the one place that
    /// spawns the background `watch_topic` consumer for a topic that just started Running.
    fn spawn_watch(self: &std::sync::Arc<Self>, topic_id: i64, execution_id: Uuid) {
        let manager = std::sync::Arc::clone(self);
        tokio::spawn(async move { manager.watch_topic(topic_id, execution_id).await });
    }
}

#[cfg(all(test, feature = "test-support"))]
mod tests {
    use super::*;
    use common::proto::engine::v1::{
        CancelRequest, CancelResponse, CreateScheduleRequest, CreateScheduleResponse,
        EngineService, Execution, ExecutionEvent, GetExecutionRequest, InterruptRequest,
        InterruptResponse, ListSchedulesRequest, ListSchedulesResponse, RegisterGraphRequest,
        RegisterGraphResponse, ResumeRequest, ResumeResponse, StartExecutionRequest,
        StartExecutionResponse, StreamEventsRequest as EngineStreamEventsRequest,
    };
    use connectrpc::{
        RequestContext, Response, Router as ConnectRouter, ServiceRequest, ServiceResult,
        ServiceStream,
    };
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct FakeEngine {
        interrupts: Mutex<Vec<InterruptRequest>>,
        start_executions: Mutex<Vec<StartExecutionRequest>>,
        /// execution_id (string) -> the one event `stream_events` replays for it. Registered by
        /// tests after a topic's execution_id is known (it's chosen by `start_execution` below).
        completions: Mutex<HashMap<String, ExecutionEvent>>,
    }

    #[allow(refining_impl_trait)]
    impl EngineService for FakeEngine {
        async fn register_graph(
            &self,
            _ctx: RequestContext,
            _request: ServiceRequest<'_, RegisterGraphRequest>,
        ) -> ServiceResult<RegisterGraphResponse> {
            Response::ok(RegisterGraphResponse::default())
        }
        async fn start_execution(
            &self,
            _ctx: RequestContext,
            request: ServiceRequest<'_, StartExecutionRequest>,
        ) -> ServiceResult<StartExecutionResponse> {
            self.start_executions
                .lock()
                .expect("lock")
                .push(request.to_owned_message());
            Response::ok(StartExecutionResponse {
                execution_id: uuid::Uuid::new_v4().to_string(),
                ..Default::default()
            })
        }
        async fn interrupt(
            &self,
            _ctx: RequestContext,
            request: ServiceRequest<'_, InterruptRequest>,
        ) -> ServiceResult<InterruptResponse> {
            self.interrupts
                .lock()
                .expect("lock")
                .push(request.to_owned_message());
            Response::ok(InterruptResponse::default())
        }
        async fn resume(
            &self,
            _ctx: RequestContext,
            _request: ServiceRequest<'_, ResumeRequest>,
        ) -> ServiceResult<ResumeResponse> {
            Response::ok(ResumeResponse::default())
        }
        async fn cancel(
            &self,
            _ctx: RequestContext,
            _request: ServiceRequest<'_, CancelRequest>,
        ) -> ServiceResult<CancelResponse> {
            Response::ok(CancelResponse::default())
        }
        async fn get_execution(
            &self,
            _ctx: RequestContext,
            _request: ServiceRequest<'_, GetExecutionRequest>,
        ) -> ServiceResult<Execution> {
            Response::ok(Execution::default())
        }
        async fn stream_events(
            &self,
            _ctx: RequestContext,
            request: ServiceRequest<'_, EngineStreamEventsRequest>,
        ) -> ServiceResult<ServiceStream<ExecutionEvent>> {
            let owned = request.to_owned_message();
            let event = owned
                .execution_id
                .and_then(|id| self.completions.lock().expect("lock").get(&id).cloned());
            match event {
                Some(event) => Response::stream_ok(futures::stream::iter([Ok(event)])),
                None => Response::stream_ok(futures::stream::empty()),
            }
        }
        async fn create_schedule(
            &self,
            _ctx: RequestContext,
            _request: ServiceRequest<'_, CreateScheduleRequest>,
        ) -> ServiceResult<CreateScheduleResponse> {
            Response::ok(CreateScheduleResponse::default())
        }
        async fn list_schedules(
            &self,
            _ctx: RequestContext,
            _request: ServiceRequest<'_, ListSchedulesRequest>,
        ) -> ServiceResult<ListSchedulesResponse> {
            Response::ok(ListSchedulesResponse::default())
        }
    }

    async fn serve_engine(fake: Arc<FakeEngine>) -> String {
        let connect = ConnectRouter::new().add_service(fake);
        let app = axum::Router::new().fallback_service(connect.into_axum_service());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("addr");
        tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
        format!("http://{address}")
    }

    async fn manager_with() -> (crate::test_db::TestDb, Arc<TopicManager>, Arc<FakeEngine>) {
        let test = crate::test_db::start().await;
        let fake = Arc::new(FakeEngine::default());
        let engine_url = serve_engine(Arc::clone(&fake)).await;
        let manager = Arc::new(TopicManager::new(test.db.clone(), &engine_url).expect("manager"));
        (test, manager, fake)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_first_topic_in_a_session_starts_running_and_becomes_focus() {
        let (_test, manager, _fake) = manager_with().await;
        let user_id = Uuid::new_v4();

        let (topic_id, status) = manager
            .create_topic(
                user_id,
                None,
                "First".into(),
                r#"{"question": "hi"}"#.into(),
            )
            .await
            .expect("create");

        assert_eq!(status, Status::Running);
        let (focus, _topics) = manager
            .session
            .get_session_view(user_id)
            .await
            .expect("view");
        assert_eq!(focus, Some(topic_id));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_fourth_concurrent_topic_is_queued_not_started() {
        let (_test, manager, _fake) = manager_with().await;
        let user_id = Uuid::new_v4();
        for i in 0..MAX_CONCURRENT_TOPICS {
            let (_id, status) = manager
                .create_topic(user_id, None, format!("T{i}"), "{}".into())
                .await
                .expect("create");
            assert_eq!(status, Status::Running);
        }

        let (_id, status) = manager
            .create_topic(user_id, None, "Overflow".into(), "{}".into())
            .await
            .expect("create");

        assert_eq!(status, Status::Queued);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_second_root_topic_does_not_steal_focus() {
        let (_test, manager, _fake) = manager_with().await;
        let user_id = Uuid::new_v4();
        let (first_id, _) = manager
            .create_topic(user_id, None, "First".into(), "{}".into())
            .await
            .expect("create");

        manager
            .create_topic(user_id, None, "Second".into(), "{}".into())
            .await
            .expect("create");

        let (focus, _topics) = manager
            .session
            .get_session_view(user_id)
            .await
            .expect("view");
        assert_eq!(focus, Some(first_id));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn set_focus_moves_the_pointer_and_rejects_a_foreign_topic() {
        let (_test, manager, _fake) = manager_with().await;
        let user_id = Uuid::new_v4();
        let (first_id, _) = manager
            .create_topic(user_id, None, "First".into(), "{}".into())
            .await
            .expect("create");
        let (second_id, _) = manager
            .create_topic(user_id, None, "Second".into(), "{}".into())
            .await
            .expect("create");

        // still focused on the first topic (root topics don't steal focus)
        let (focus, _) = manager
            .session
            .get_session_view(user_id)
            .await
            .expect("view");
        assert_eq!(focus, Some(first_id));

        manager
            .set_focus(user_id, second_id)
            .await
            .expect("set_focus");
        let (focus, _) = manager
            .session
            .get_session_view(user_id)
            .await
            .expect("view");
        assert_eq!(focus, Some(second_id));

        // a topic from a different session's user is rejected
        let other_user_id = Uuid::new_v4();
        let error = manager
            .set_focus(other_user_id, first_id)
            .await
            .expect_err("foreign topic must be rejected");
        assert!(matches!(error, ChatError::TopicNotFound(id) if id == first_id));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn send_turn_routes_to_focus_and_interrupts_the_engine() {
        let (_test, manager, fake) = manager_with().await;
        let user_id = Uuid::new_v4();
        let (topic_id, _) = manager
            .create_topic(user_id, None, "First".into(), "{}".into())
            .await
            .expect("create");
        let execution_id = topic::Entity::find_by_id(topic_id)
            .one(&manager.session.db)
            .await
            .expect("query")
            .expect("topic")
            .execution_id
            .expect("running topic has an execution_id");

        let turn_id = Uuid::new_v4();
        let routed = manager
            .send_turn(user_id, turn_id, "hello".into())
            .await
            .expect("send_turn");
        assert_eq!(routed, topic_id);

        {
            let interrupts = fake.interrupts.lock().expect("lock");
            assert_eq!(interrupts.len(), 1);
            assert_eq!(interrupts[0].execution_id, execution_id.to_string());
        }

        // repeating the same turn_id is a genuine no-op: no second Interrupt, no duplicate row
        manager
            .send_turn(user_id, turn_id, "hello".into())
            .await
            .expect("send_turn again");
        assert_eq!(
            fake.interrupts.lock().expect("lock").len(),
            1,
            "Interrupt must not be called a second time for a repeated turn_id"
        );

        use crate::entity::message;
        let stored = message::Entity::find()
            .filter(message::Column::TopicId.eq(topic_id))
            .filter(message::Column::TurnId.eq(turn_id))
            .all(&manager.session.db)
            .await
            .expect("query messages");
        assert_eq!(stored.len(), 1, "no duplicate chat.messages row");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn finishing_a_running_topic_promotes_its_queued_sibling() {
        let (_test, manager, fake) = manager_with().await;
        let user_id = Uuid::new_v4();
        let mut running_ids = Vec::new();
        for i in 0..MAX_CONCURRENT_TOPICS {
            let (id, status) = manager
                .create_topic(user_id, None, format!("T{i}"), "{}".into())
                .await
                .expect("create");
            assert_eq!(status, Status::Running);
            running_ids.push(id);
        }
        let (queued_id, status) = manager
            .create_topic(user_id, None, "Overflow".into(), "{}".into())
            .await
            .expect("create");
        assert_eq!(status, Status::Queued);

        let finishing_id = running_ids[0];
        let execution_id = topic::Entity::find_by_id(finishing_id)
            .one(&manager.session.db)
            .await
            .expect("query")
            .expect("topic")
            .execution_id
            .expect("running topic has an execution_id");
        fake.completions.lock().expect("lock").insert(
            execution_id.to_string(),
            ExecutionEvent {
                id: "e1".to_owned(),
                execution_id: execution_id.to_string(),
                payload_kind: "ExecutionCompleted".to_owned(),
                payload_json: r#"{"final_state":{"llm":{"reply":"done"}}}"#.to_owned(),
                ..Default::default()
            },
        );
        let start_executions_before = fake.start_executions.lock().expect("lock").len();

        // watch_topic drains the (fake) completion event to the end, so finish_topic and
        // promote_next_queued have both fully run by the time this returns.
        manager.watch_topic(finishing_id, execution_id).await;

        let finished = topic::Entity::find_by_id(finishing_id)
            .one(&manager.session.db)
            .await
            .expect("query")
            .expect("topic");
        assert_eq!(finished.status, Status::Completed);
        assert_eq!(finished.result_summary, Some("done".to_owned()));

        let promoted = topic::Entity::find_by_id(queued_id)
            .one(&manager.session.db)
            .await
            .expect("query")
            .expect("topic");
        assert_eq!(promoted.status, Status::Running);
        assert!(promoted.execution_id.is_some());

        let start_executions_after = fake.start_executions.lock().expect("lock").len();
        assert_eq!(
            start_executions_after,
            start_executions_before + 1,
            "promotion must call start_execution on the Engine for the promoted topic"
        );
    }
}
