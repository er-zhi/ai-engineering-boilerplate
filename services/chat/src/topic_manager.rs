// create_topic: the entry point for a new topic, root or child. Concurrency-limited per
// session (spec's default of 3 running at once) — beyond the limit, a topic is Queued instead
// of started; Task 8's completion handler promotes the oldest Queued topic when a slot frees.

use chrono::Utc;
use common::proto::chat::v1::ChatEvent;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, DatabaseConnection, EntityTrait,
    PaginatorTrait, QueryFilter, QueryOrder, QuerySelect, TransactionTrait,
};
use uuid::Uuid;

use crate::engine_client::EngineClient;
use crate::entity::session;
use crate::entity::topic::{self, Status};
use crate::error::ChatError;
use crate::events::EventBus;
use crate::session_manager::SessionManager;

const AGENT_GRAPH_ID: &str = "agent";
pub const MAX_CONCURRENT_TOPICS: u64 = 3;

/// `SELECT ... FROM chat.sessions WHERE id = $1 FOR UPDATE` — the one serialization point for
/// everything that changes how many topics are Running in a session (`create_topic`'s
/// slot-limit check and `promote_next_queued`'s pick-then-start). Postgres holds the row lock
/// until the enclosing transaction ends, so a second caller for the *same* session waits here
/// while a different session proceeds untouched.
async fn lock_session<C: sea_orm::ConnectionTrait>(
    db: &C,
    session_id: Uuid,
) -> Result<session::Model, ChatError> {
    session::Entity::find_by_id(session_id)
        .lock_exclusive()
        .one(db)
        .await?
        .ok_or_else(|| ChatError::InvalidRequest("session vanished".to_owned()))
}

pub struct TopicManager {
    pub session: SessionManager,
    pub(crate) engine: EngineClient,
    pub events: EventBus,
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
        let row = self
            .insert_topic(session.id, parent_id, title, &input_json)
            .await?;

        self.events.publish(ChatEvent {
            topic_id: row.id.to_string(),
            kind: if row.status == Status::Running {
                "topic_started"
            } else {
                "topic_queued"
            }
            .to_owned(),
            occurred_at: row.created_at.to_rfc3339(),
            ..Default::default()
        });

        if let (Status::Running, Some(execution_id)) = (row.status, row.execution_id) {
            self.spawn_watch(row.id, execution_id);
        }

        Ok((row.id, row.status))
    }

    /// `create_topic`'s one atomic decision: how many topics are Running, whether this one gets
    /// a slot, starting its execution if it does, and inserting the row — plus the spec's "the
    /// first topic in a session becomes focus" rule.
    ///
    /// All of it under the session's row lock, because the count and the insert are otherwise
    /// two unsynchronized statements: concurrent `CreateTopic` calls for one session could each
    /// see `running < MAX_CONCURRENT_TOPICS` before any of them inserted, and the cap would be
    /// breached. Every path that changes how many topics are Running takes this same lock.
    async fn insert_topic(
        &self,
        session_id: Uuid,
        parent_id: Option<i64>,
        title: String,
        input_json: &str,
    ) -> Result<topic::Model, ChatError> {
        let txn = self.session.db.begin().await?;
        let locked = lock_session(&txn, session_id).await?;
        let running = topic::Entity::find()
            .filter(topic::Column::SessionId.eq(session_id))
            .filter(topic::Column::Status.eq(Status::Running))
            .count(&txn)
            .await?;

        let status = if running < MAX_CONCURRENT_TOPICS {
            Status::Running
        } else {
            Status::Queued
        };
        let execution_id = if status == Status::Running {
            Some(
                self.engine
                    .start_execution(AGENT_GRAPH_ID, input_json)
                    .await
                    .map_err(ChatError::Engine)?,
            )
        } else {
            None
        };

        let now = Utc::now();
        let row = topic::ActiveModel {
            session_id: Set(session_id),
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
        .insert(&txn)
        .await?;

        // Read off the *locked* row, not an earlier snapshot, so two concurrent first-topic
        // creations can't both claim focus.
        if locked.focus_topic_id.is_none() {
            let mut active: session::ActiveModel = locked.into();
            active.focus_topic_id = Set(Some(row.id));
            active.update(&txn).await?;
        }
        txn.commit().await?;
        Ok(row)
    }

    pub async fn set_focus(&self, user_id: Uuid, topic_id: i64) -> Result<(), ChatError> {
        let session = self.session.get_or_create_session(user_id).await?;
        let topic = topic::Entity::find_by_id(topic_id)
            .one(&self.session.db)
            .await?
            .filter(|t| t.session_id == session.id)
            .ok_or(ChatError::TopicNotFound(topic_id))?;

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

        // The dedup check stays up front, so a retry *after* a successful delivery short-circuits
        // without interrupting the execution a second time.
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

        // `execution_id` stays set forever once a topic reaches a terminal status, so its
        // presence says nothing about whether the execution is still there to interrupt. Only a
        // Running topic can receive a turn; anything else is rejected outright rather than
        // routed into a doomed Interrupt. (Auto-creating a child topic for a finished one is
        // the spec's eventual answer and is deliberately out of scope here.)
        if topic.status != Status::Running {
            return Err(ChatError::TopicNotRunning(topic_id));
        }
        let execution_id = topic.execution_id.ok_or(ChatError::InvalidRequest(format!(
            "topic {topic_id} has no running execution to interrupt"
        )))?;
        let input_json = serde_json::json!({"question": content}).to_string();
        self.engine
            .interrupt(execution_id, &input_json)
            .await
            .map_err(ChatError::Engine)?;

        // Only now is the turn actually delivered, so only now is the dedup marker written. The
        // other order loses the turn for good on a transient Interrupt failure: the client's
        // documented same-turn_id retry would find the row and report success as a no-op.
        message::ActiveModel {
            topic_id: Set(topic_id),
            turn_id: Set(turn_id),
            content: Set(content),
            created_at: Set(Utc::now()),
            ..Default::default()
        }
        .insert(&self.session.db)
        .await?;
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
            self.handle_engine_event_logged(topic_id, &event).await;
        }
    }

    /// `watch_topic`'s per-event step. Split out of the loop body so a failed event doesn't
    /// abort the stream: there is no caller to propagate to (the watcher is a background task),
    /// so the error is logged and the next event is taken.
    async fn handle_engine_event_logged(
        self: &std::sync::Arc<Self>,
        topic_id: i64,
        event: &common::proto::engine::v1::ExecutionEvent,
    ) {
        if let Err(error) = self.handle_engine_event(topic_id, event).await {
            tracing::error!(topic_id, %error, "failed to handle an Engine event");
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
        // Engine puts the whole externally-tagged envelope into `payload_json` — e.g.
        // `{"ExecutionCompleted": {"final_state": ..}}` — and repeats the outer key in
        // `payload_kind` (see services/engine/src/stream.rs::row_to_proto). Unwrap that envelope
        // before reading the body; reading `final_state`/`error` off the top level always misses.
        let payload: serde_json::Value =
            serde_json::from_str(&event.payload_json).unwrap_or(serde_json::Value::Null);
        let body = payload.get(event.payload_kind.as_str()).unwrap_or(&payload);
        let summary = body
            .get("final_state")
            .and_then(|state| state.pointer("/llm/reply"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .or_else(|| {
                body.get("error")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            });

        let (row, focus_change) = self
            .update_topic_status_and_move_focus(topic_id, status, summary.clone())
            .await?;

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

        if let Some((next, from)) = focus_change {
            self.events.publish(ChatEvent {
                topic_id: next.map(|id| id.to_string()).unwrap_or_default(),
                kind: "focus_changed".to_owned(),
                payload_json: serde_json::json!({
                    "from": from,
                    "to": next,
                    "reason": "completion",
                })
                .to_string(),
                occurred_at: Utc::now().to_rfc3339(),
                ..Default::default()
            });
        }

        self.promote_next_queued(row.session_id).await
    }

    /// `finish_topic`'s one atomic decision: mark the topic terminal and, when it was the
    /// completing topic's completion and it held focus, move focus off it — both under the
    /// session's row lock, the same serialization point `create_topic` and `promote_next_queued`
    /// use.
    ///
    /// Without that lock, `move_focus_off_completed`'s select-next + update-focus could
    /// interleave with a sibling topic's own status commit (two `finish_topic` calls racing) or
    /// with a concurrent `create_topic` committing a new topic between the select and the write —
    /// leaving focus dangling on a finished topic, or cleared while an unfinished topic exists.
    /// Joining the focus move into this same transaction (rather than opening a second one, which
    /// `finish_topic` didn't have to begin with — its status update ran in plain autocommit)
    /// avoids a second round trip and the window that would otherwise sit between them.
    async fn update_topic_status_and_move_focus(
        &self,
        topic_id: i64,
        status: Status,
        summary: Option<String>,
    ) -> Result<(topic::Model, Option<(Option<i64>, i64)>), ChatError> {
        // `session_id` is immutable once a topic is created (nothing ever reassigns a topic to a
        // different session), so this unlocked read is safe — it's only used to know which
        // session row to lock next.
        let session_id = topic::Entity::find_by_id(topic_id)
            .one(&self.session.db)
            .await?
            .ok_or(ChatError::TopicNotFound(topic_id))?
            .session_id;

        let txn = self.session.db.begin().await?;
        let locked_session = lock_session(&txn, session_id).await?;

        let mut active: topic::ActiveModel = topic::Entity::find_by_id(topic_id)
            .one(&txn)
            .await?
            .ok_or(ChatError::TopicNotFound(topic_id))?
            .into();
        active.status = Set(status);
        active.result_summary = Set(summary);
        active.updated_at = Set(Utc::now());
        let row = active.update(&txn).await?;

        let focus_change = if status == Status::Completed {
            self.move_focus_off_completed(&txn, locked_session, topic_id)
                .await?
        } else {
            None
        };
        txn.commit().await?;
        Ok((row, focus_change))
    }

    /// Spec: «фокусная тема завершилась → фокус переходит на следующую незавершённую» — when the
    /// focused topic completes, focus moves to the next unfinished topic in the session (oldest
    /// first, matching `promote_next_queued`'s ordering), or is cleared if none remain.
    ///
    /// Completion only. The spec is explicit that a failed or cancelled topic leaves the pointer
    /// where it is («фокус не меняется», «фокус не трогаем»), and `send_turn` now rejects a turn
    /// aimed at a finished topic with a clear message rather than misrouting it, so the user is
    /// told what happened instead of silently losing the reply.
    ///
    /// Called from within `finish_topic`'s locked transaction, on the session row it already
    /// holds exclusively (`session` here is that locked row, not a fresh read) — so this
    /// select-next-unfinished + update-focus sequence can't race a concurrent `create_topic`
    /// (which takes the same lock before deciding whether to claim focus for a brand-new
    /// session) or a sibling topic's own `finish_topic` call for this session. Returns
    /// `Some((next, from))` for the caller to publish as `focus_changed` once the transaction
    /// commits, or `None` if the session wasn't focused on `completed_id` to begin with (the
    /// user had already looked away, so there's nothing to move).
    async fn move_focus_off_completed<C: sea_orm::ConnectionTrait>(
        &self,
        txn: &C,
        session: session::Model,
        completed_id: i64,
    ) -> Result<Option<(Option<i64>, i64)>, ChatError> {
        if session.focus_topic_id != Some(completed_id) {
            return Ok(None); // the user was looking at something else; don't move them
        }

        let next = topic::Entity::find()
            .filter(topic::Column::SessionId.eq(session.id))
            .filter(topic::Column::Status.is_in([Status::Running, Status::Queued]))
            .order_by_asc(topic::Column::CreatedAt)
            .one(txn)
            .await?
            .map(|topic| topic.id);
        let mut active: session::ActiveModel = session.into();
        active.focus_topic_id = Set(next);
        active.update(txn).await?;

        Ok(Some((next, completed_id)))
    }

    /// Called after any topic leaves `Running` — starts the oldest still-`Queued` topic in the
    /// same session, if the slot that just freed leaves room (it always does: one topic just
    /// left `Running`). Returns `Ok(())` with nothing promoted if there's no queued topic.
    async fn promote_next_queued(
        self: &std::sync::Arc<Self>,
        session_id: Uuid,
    ) -> Result<(), ChatError> {
        // Same serialization point as `create_topic`: picking the queued topic and marking it
        // Running are two statements, and two `finish_topic` calls racing in the same session
        // would otherwise both pick the same row — starting two Engine executions for one topic
        // and leaving the second-oldest queued topic behind. Holding the lock across
        // `start_execution` keeps a network call inside a transaction, which is a smell; at this
        // scale (one session, one short call) it is the right trade against a two-phase design.
        let txn = self.session.db.begin().await?;
        let _locked = lock_session(&txn, session_id).await?;
        let Some(next) = topic::Entity::find()
            .filter(topic::Column::SessionId.eq(session_id))
            .filter(topic::Column::Status.eq(Status::Queued))
            .order_by_asc(topic::Column::CreatedAt)
            .one(&txn)
            .await?
        else {
            txn.commit().await?; // nothing queued — release the session lock right away
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
        active.update(&txn).await?;
        txn.commit().await?;

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
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct FakeEngine {
        /// Every attempt, failed ones included.
        interrupts: Mutex<Vec<InterruptRequest>>,
        /// How many of the next `interrupt` calls fail — stands in for a transient Engine
        /// outage, which is the case the turn_id dedup marker must survive.
        interrupt_failures_remaining: AtomicUsize,
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
            if self
                .interrupt_failures_remaining
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_ok()
            {
                return Err(connectrpc::ConnectError::unavailable(
                    "configured fake engine interrupt failure",
                ));
            }
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
                // The real Engine's envelope shape, verbatim (stream.rs::row_to_proto sends the
                // whole externally-tagged payload, not the inner body).
                payload_json: r#"{"ExecutionCompleted":{"final_state":{"llm":{"reply":"done"}}}}"#
                    .to_owned(),
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

    /// Registers the completion (or failure) event `watch_topic` will replay for a topic, and
    /// returns the topic's `execution_id`.
    async fn arm_terminal_event(
        manager: &Arc<TopicManager>,
        fake: &FakeEngine,
        topic_id: i64,
        payload_kind: &str,
        payload_json: &str,
    ) -> Uuid {
        let execution_id = topic::Entity::find_by_id(topic_id)
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
                payload_kind: payload_kind.to_owned(),
                payload_json: payload_json.to_owned(),
                ..Default::default()
            },
        );
        execution_id
    }

    async fn focus_of(manager: &Arc<TopicManager>, user_id: Uuid) -> Option<i64> {
        manager
            .session
            .get_session_view(user_id)
            .await
            .expect("view")
            .0
    }

    async fn set_status(manager: &Arc<TopicManager>, topic_id: i64, status: Status) {
        let mut active: topic::ActiveModel = topic::Entity::find_by_id(topic_id)
            .one(&manager.session.db)
            .await
            .expect("query")
            .expect("topic")
            .into();
        active.status = Set(status);
        active.update(&manager.session.db).await.expect("update");
    }

    /// The finding: `send_turn` only checked that `execution_id` was set, and that column stays
    /// set forever after a topic finishes — so a turn aimed at a dead focus topic was routed
    /// into an `Interrupt` on an execution that had already ended. It must be refused instead,
    /// and refused before any Engine call is made.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_turn_for_a_finished_focus_topic_is_refused_rather_than_interrupted() {
        let (_test, manager, fake) = manager_with().await;
        let user_id = Uuid::new_v4();
        let (topic_id, _) = manager
            .create_topic(user_id, None, "First".into(), "{}".into())
            .await
            .expect("create");
        set_status(&manager, topic_id, Status::Completed).await;

        let error = manager
            .send_turn(user_id, Uuid::new_v4(), "hello".into())
            .await
            .expect_err("a completed focus topic must not accept a turn");

        assert!(matches!(error, ChatError::TopicNotRunning(id) if id == topic_id));
        assert!(
            fake.interrupts.lock().expect("lock").is_empty(),
            "no Interrupt may be attempted against a finished execution"
        );
        let error: connectrpc::ConnectError = error.into();
        assert_eq!(error.code, connectrpc::ErrorCode::FailedPrecondition);
    }

    /// The finding: the dedup row was written before `interrupt` was called, so a transient
    /// Engine failure lost the turn for good — the client's documented same-`turn_id` retry
    /// found the row and reported success without ever delivering anything.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_turn_whose_interrupt_failed_can_be_retried_with_the_same_turn_id() {
        let (_test, manager, fake) = manager_with().await;
        let user_id = Uuid::new_v4();
        let (topic_id, _) = manager
            .create_topic(user_id, None, "First".into(), "{}".into())
            .await
            .expect("create");
        fake.interrupt_failures_remaining
            .store(1, Ordering::Relaxed);
        let turn_id = Uuid::new_v4();

        let error = manager
            .send_turn(user_id, turn_id, "hello".into())
            .await
            .expect_err("the first attempt fails inside Engine");
        assert!(matches!(error, ChatError::Engine(_)));

        use crate::entity::message;
        let stored = message::Entity::find()
            .filter(message::Column::TopicId.eq(topic_id))
            .filter(message::Column::TurnId.eq(turn_id))
            .all(&manager.session.db)
            .await
            .expect("query messages");
        assert!(
            stored.is_empty(),
            "an undelivered turn must leave no dedup marker, or the retry becomes a silent no-op"
        );

        let routed = manager
            .send_turn(user_id, turn_id, "hello".into())
            .await
            .expect("the retry with the same turn_id must actually deliver");

        assert_eq!(routed, topic_id);
        assert_eq!(
            fake.interrupts.lock().expect("lock").len(),
            2,
            "the retry must reach Engine, not short-circuit on a marker from the failed attempt"
        );
        let stored = message::Entity::find()
            .filter(message::Column::TopicId.eq(topic_id))
            .filter(message::Column::TurnId.eq(turn_id))
            .all(&manager.session.db)
            .await
            .expect("query messages");
        assert_eq!(stored.len(), 1, "exactly one dedup row after the retry");
    }

    /// The finding: nothing ever moved `session.focus_topic_id` off a topic that finished, so
    /// the user was left pointed at a dead topic. Spec: «фокусная тема завершилась → фокус
    /// переходит на следующую незавершённую».
    #[tokio::test(flavor = "multi_thread")]
    async fn focus_moves_to_the_next_unfinished_topic_when_the_focused_one_completes() {
        let (_test, manager, fake) = manager_with().await;
        let user_id = Uuid::new_v4();
        let (first_id, _) = manager
            .create_topic(user_id, None, "First".into(), "{}".into())
            .await
            .expect("create");
        let (second_id, _) = manager
            .create_topic(user_id, None, "Second".into(), "{}".into())
            .await
            .expect("create");
        assert_eq!(focus_of(&manager, user_id).await, Some(first_id));

        let execution_id = arm_terminal_event(
            &manager,
            &fake,
            first_id,
            "ExecutionCompleted",
            r#"{"ExecutionCompleted":{"final_state":{"llm":{"reply":"done"}}}}"#,
        )
        .await;
        manager.watch_topic(first_id, execution_id).await;

        assert_eq!(
            focus_of(&manager, user_id).await,
            Some(second_id),
            "focus must follow on to the next unfinished topic"
        );

        // …and is cleared once nothing unfinished is left.
        let execution_id = arm_terminal_event(
            &manager,
            &fake,
            second_id,
            "ExecutionCompleted",
            r#"{"ExecutionCompleted":{"final_state":{"llm":{"reply":"also done"}}}}"#,
        )
        .await;
        manager.watch_topic(second_id, execution_id).await;

        assert_eq!(
            focus_of(&manager, user_id).await,
            None,
            "with no unfinished topic left, focus is cleared"
        );
    }

    /// The other half of the spec's focus rule, which is easy to over-apply: a *failed* topic
    /// keeps the focus where it is («Падение темы: ... фокус не трогаем»). `send_turn`'s
    /// `failed_precondition` is what tells the user, not a silent pointer move.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_focus_topic_keeps_the_focus_where_it_is() {
        let (_test, manager, fake) = manager_with().await;
        let user_id = Uuid::new_v4();
        let (first_id, _) = manager
            .create_topic(user_id, None, "First".into(), "{}".into())
            .await
            .expect("create");
        manager
            .create_topic(user_id, None, "Second".into(), "{}".into())
            .await
            .expect("create");

        let execution_id = arm_terminal_event(
            &manager,
            &fake,
            first_id,
            "ExecutionFailed",
            r#"{"ExecutionFailed":{"error":"boom"}}"#,
        )
        .await;
        manager.watch_topic(first_id, execution_id).await;

        assert_eq!(focus_of(&manager, user_id).await, Some(first_id));
    }

    /// The finding: `COUNT(Running)` and the `INSERT` were two unsynchronized statements, so
    /// concurrent `CreateTopic` calls for one session could all pass the check before any of
    /// them committed and blow past the cap. Sequentially this always passed; only real
    /// concurrency shows it.
    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_create_topic_calls_cannot_exceed_the_running_limit() {
        let (_test, manager, _fake) = manager_with().await;
        let user_id = Uuid::new_v4();
        // Create the session up front: the race under test is the slot check, not session
        // creation (which the unique constraint on user_id already serializes).
        let session_id = manager
            .session
            .get_or_create_session(user_id)
            .await
            .expect("session")
            .id;

        let attempts = 2 * MAX_CONCURRENT_TOPICS;
        let mut tasks = Vec::new();
        for i in 0..attempts {
            let manager = Arc::clone(&manager);
            tasks.push(tokio::spawn(async move {
                manager
                    .create_topic(user_id, None, format!("T{i}"), "{}".into())
                    .await
                    .expect("create")
            }));
        }
        for task in tasks {
            task.await.expect("join");
        }

        let running = topic::Entity::find()
            .filter(topic::Column::SessionId.eq(session_id))
            .filter(topic::Column::Status.eq(Status::Running))
            .count(&manager.session.db)
            .await
            .expect("count");
        assert_eq!(
            running, MAX_CONCURRENT_TOPICS,
            "the per-session concurrency cap must hold under concurrent creation"
        );
        let total = topic::Entity::find()
            .filter(topic::Column::SessionId.eq(session_id))
            .count(&manager.session.db)
            .await
            .expect("count");
        assert_eq!(total, attempts, "every attempt is recorded, queued or not");
    }

    /// The finding: two topics finishing at once both ran `promote_next_queued`, both selected
    /// the same oldest `Queued` row, and both started an Engine execution for it — one topic,
    /// two executions, and the row updated twice.
    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_promotions_cannot_start_the_same_queued_topic_twice() {
        let (_test, manager, fake) = manager_with().await;
        let user_id = Uuid::new_v4();
        for i in 0..MAX_CONCURRENT_TOPICS {
            manager
                .create_topic(user_id, None, format!("T{i}"), "{}".into())
                .await
                .expect("create");
        }
        let (queued_id, status) = manager
            .create_topic(user_id, None, "Overflow".into(), "{}".into())
            .await
            .expect("create");
        assert_eq!(status, Status::Queued);
        let session_id = manager
            .session
            .get_or_create_session(user_id)
            .await
            .expect("session")
            .id;
        let before = fake.start_executions.lock().expect("lock").len();

        let (first, second) = tokio::join!(
            {
                let manager = Arc::clone(&manager);
                async move { manager.promote_next_queued(session_id).await }
            },
            {
                let manager = Arc::clone(&manager);
                async move { manager.promote_next_queued(session_id).await }
            },
        );
        first.expect("promote");
        second.expect("promote");

        assert_eq!(
            fake.start_executions.lock().expect("lock").len(),
            before + 1,
            "the one queued topic must be started exactly once, not once per concurrent promoter"
        );
        let promoted = topic::Entity::find_by_id(queued_id)
            .one(&manager.session.db)
            .await
            .expect("query")
            .expect("topic");
        assert_eq!(promoted.status, Status::Running);
    }

    /// Builds the terminal event `finish_topic` needs to complete a topic; the `execution_id`
    /// inside it is irrelevant to `finish_topic` (it only reads `payload_kind`/`payload_json`),
    /// so each call gets a fresh one rather than needing the caller to look one up.
    fn completed_event() -> ExecutionEvent {
        ExecutionEvent {
            id: "e1".to_owned(),
            execution_id: Uuid::new_v4().to_string(),
            payload_kind: "ExecutionCompleted".to_owned(),
            payload_json: r#"{"ExecutionCompleted":{"final_state":{"llm":{"reply":"done"}}}}"#
                .to_owned(),
            ..Default::default()
        }
    }

    /// The finding the re-reviewer caught after Fixes 6 and 7 (which locked `create_topic` and
    /// `promote_next_queued`): `move_focus_off_completed` ran its select-next-unfinished +
    /// update-focus sequence outside any lock, so a concurrent `create_topic` for the same
    /// session could commit a brand-new topic *between* that select and that update — the
    /// "mirror case" from the finding: focus ends up cleared (or stale) even though an unfinished
    /// topic exists, because the select ran before the new topic existed and the write landed
    /// after.
    ///
    /// This is exactly the same kind of real-network-timing race Fix 6's test caught (see
    /// `.superpowers/sdd/2026-09-15-chat-service/task-12-gate-fix-report.md`) — and, like Fix 7's
    /// honest caveat there, a single trial is not guaranteed to land in the narrow window. Run
    /// many independent trials and assert the invariant holds in every one: with the session
    /// properly locked across both operations, the outcome is deterministic (focus always lands
    /// on the one remaining unfinished topic), so this test does not flake on a fix that works —
    /// a single missed lock should make at least one of these iterations fail.
    #[tokio::test(flavor = "multi_thread")]
    async fn move_focus_off_completed_cannot_race_a_concurrent_create_topic() {
        let (_test, manager, _fake) = manager_with().await;

        for i in 0..25 {
            let user_id = Uuid::new_v4();
            let (focus_id, status) = manager
                .create_topic(user_id, None, format!("Focus{i}"), "{}".into())
                .await
                .expect("create focus topic");
            assert_eq!(status, Status::Running);

            let finisher = Arc::clone(&manager);
            let creator = Arc::clone(&manager);
            let (finish_result, create_result) = tokio::join!(
                async move {
                    finisher
                        .finish_topic(focus_id, Status::Completed, &completed_event())
                        .await
                },
                async move {
                    creator
                        .create_topic(user_id, None, format!("Sibling{i}"), "{}".into())
                        .await
                },
            );
            finish_result.expect("finish_topic");
            let (sibling_id, sibling_status) = create_result.expect("create sibling topic");
            assert_eq!(sibling_status, Status::Running);

            let focus = focus_of(&manager, user_id).await;
            assert_eq!(
                focus,
                Some(sibling_id),
                "iteration {i}: focus must land on the only unfinished topic (the sibling), \
                 not be lost to the create_topic race"
            );
        }
    }

    /// Guards the envelope contract on the failure path: Engine sends
    /// `{"ExecutionFailed":{"error": ..}}`, and that error text must land in `result_summary`
    /// rather than being silently dropped.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_execution_records_the_engine_error_as_the_summary() {
        let (_test, manager, fake) = manager_with().await;
        let user_id = Uuid::new_v4();
        let (topic_id, _) = manager
            .create_topic(user_id, None, "Rust news".into(), "{}".into())
            .await
            .expect("create");
        let execution_id = topic::Entity::find_by_id(topic_id)
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
                payload_kind: "ExecutionFailed".to_owned(),
                payload_json: r#"{"ExecutionFailed":{"error":"brave search returned 422"}}"#
                    .to_owned(),
                ..Default::default()
            },
        );

        manager.watch_topic(topic_id, execution_id).await;

        let finished = topic::Entity::find_by_id(topic_id)
            .one(&manager.session.db)
            .await
            .expect("query")
            .expect("topic");
        assert_eq!(finished.status, Status::Failed);
        assert_eq!(
            finished.result_summary,
            Some("brave search returned 422".to_owned())
        );
    }
}
