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

use crate::classifier::{Action, TopicClassifier, TopicSummary};
use crate::engine_client::EngineClient;
use crate::entity::session;
use crate::entity::topic::{self, Status};
use crate::error::ChatError;
use crate::event_log::event;
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

/// `watch_topic`'s starting backoff between reconnect attempts — doubled each retry, capped at
/// `RECONNECT_MAX_DELAY`. 1s keeps a genuinely brief blip (Engine's own restart, typically a few
/// seconds) from stalling the topic noticeably, while the cap keeps a prolonged outage from
/// hammering Engine every reconnect.
const RECONNECT_BASE_DELAY: std::time::Duration = std::time::Duration::from_secs(1);
const RECONNECT_MAX_DELAY: std::time::Duration = std::time::Duration::from_secs(30);

pub struct TopicManager {
    pub session: SessionManager,
    pub(crate) engine: EngineClient,
    pub events: EventBus,
    pub(crate) classifier: TopicClassifier,
    /// `watch_topic`'s reconnect backoff — real values in production, shrunk by tests so a
    /// reconnect test doesn't have to wait out a real 1s sleep.
    reconnect_base_delay: std::time::Duration,
    reconnect_max_delay: std::time::Duration,
}

impl TopicManager {
    pub fn new(
        db: DatabaseConnection,
        engine_url: &str,
        llm_router_url: &str,
    ) -> Result<Self, String> {
        Ok(Self {
            session: SessionManager::new(db),
            engine: EngineClient::new(engine_url)?,
            events: EventBus::default(),
            classifier: TopicClassifier::new(llm_router_url)?,
            reconnect_base_delay: RECONNECT_BASE_DELAY,
            reconnect_max_delay: RECONNECT_MAX_DELAY,
        })
    }

    /// Test-only: shrinks the reconnect backoff so a test exercising `watch_topic`'s reconnect
    /// loop doesn't have to wait out the real 1s..30s delays.
    #[cfg(feature = "test-support")]
    pub fn with_reconnect_delays(
        mut self,
        base: std::time::Duration,
        max: std::time::Duration,
    ) -> Self {
        self.reconnect_base_delay = base;
        self.reconnect_max_delay = max;
        self
    }

    /// Every lifecycle event goes through here: appended to `chat.events` **and** broadcast to
    /// whatever `StreamEvents` calls are attached right now. The stored row is what a client that
    /// reloads replays; the broadcast is what an already-connected client sees live.
    ///
    /// A failed insert is logged, never propagated: losing the durable copy of a `topic_started`
    /// must not fail the topic that just started. The live event still goes out.
    async fn publish(&self, session_id: Uuid, event: ChatEvent) {
        let payload_json: serde_json::Value = serde_json::from_str(&event.payload_json)
            .unwrap_or_else(|_| serde_json::json!({"text": event.payload_json}));
        let occurred_at = chrono::DateTime::parse_from_rfc3339(&event.occurred_at)
            .map(|at| at.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now());
        let stored = event::ActiveModel {
            session_id: Set(session_id),
            topic_id: Set(event.topic_id.parse::<i64>().ok()),
            kind: Set(event.kind.clone()),
            payload_json: Set(payload_json),
            occurred_at: Set(occurred_at),
            ..Default::default()
        }
        .insert(&self.session.db)
        .await;
        if let Err(error) = stored {
            tracing::error!(%error, kind = %event.kind, "failed to persist a chat event");
        }
        self.events.publish(ChatEvent {
            session_id: session_id.to_string(),
            ..event
        });
    }

    /// Every stored event for a session, oldest first — `StreamEvents`' replay.
    pub async fn stored_events(&self, session_id: Uuid) -> Result<Vec<ChatEvent>, ChatError> {
        let rows = event::Entity::find()
            .filter(event::Column::SessionId.eq(session_id))
            .order_by_asc(event::Column::Id)
            .all(&self.session.db)
            .await?;
        Ok(rows
            .into_iter()
            .map(|row| ChatEvent {
                topic_id: row.topic_id.map(|id| id.to_string()).unwrap_or_default(),
                kind: row.kind,
                payload_json: row.payload_json.to_string(),
                occurred_at: row.occurred_at.to_rfc3339(),
                session_id: session_id.to_string(),
                ..Default::default()
            })
            .collect())
    }

    /// `session_id` is immutable once a topic is created, so this is a plain unlocked read —
    /// it only answers "which session does this event belong to".
    async fn session_of_topic(&self, topic_id: i64) -> Result<Uuid, ChatError> {
        Ok(topic::Entity::find_by_id(topic_id)
            .one(&self.session.db)
            .await?
            .ok_or(ChatError::TopicNotFound(topic_id))?
            .session_id)
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

        // `topic_created` first, then how it was admitted: the replay is the only thing a
        // reloading client has, so the event that says a topic exists (and what it is about) has
        // to be in it, not just the status transition.
        self.publish(
            session.id,
            ChatEvent {
                topic_id: row.id.to_string(),
                kind: "topic_created".to_owned(),
                payload_json: serde_json::json!({
                    "title": row.title,
                    "parent_id": parent_id,
                })
                .to_string(),
                occurred_at: row.created_at.to_rfc3339(),
                ..Default::default()
            },
        )
        .await;
        self.publish(
            session.id,
            ChatEvent {
                topic_id: row.id.to_string(),
                kind: if row.status == Status::Running {
                    "topic_started"
                } else {
                    "topic_queued"
                }
                .to_owned(),
                occurred_at: row.created_at.to_rfc3339(),
                ..Default::default()
            },
        )
        .await;

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
            input_json: Set(input_json.to_owned()),
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

        self.publish(
            session.id,
            ChatEvent {
                topic_id: topic.id.to_string(),
                kind: "focus_changed".to_owned(),
                payload_json:
                    serde_json::json!({"from": previous, "to": topic_id, "reason": "user"})
                        .to_string(),
                occurred_at: Utc::now().to_rfc3339(),
                ..Default::default()
            },
        )
        .await;
        Ok(())
    }

    /// A user turn, routed by the LLM classifier rather than by a hand-made topic choice.
    ///
    /// Returns every topic the turn was delivered to, first one first — `SendTurnResponse`'s
    /// `topic_id` is that first entry and `topic_ids` the whole list. A first message naming two
    /// themes opens two topics here; a mid-conversation aside opens one more beside the running
    /// one; anything else continues an existing topic, which is still the common case.
    pub async fn send_turn(
        self: &std::sync::Arc<Self>,
        user_id: Uuid,
        turn_id: Uuid,
        content: String,
    ) -> Result<Vec<i64>, ChatError> {
        let (focus_topic_id, topics) = self.session.get_session_view(user_id).await?;

        // The dedup check stays up front, so a retry *after* a successful delivery short-circuits
        // without interrupting the execution a second time.
        //
        // It is keyed on the turn across the *whole session*, never on `(focus topic, turn_id)`:
        // focus is not stable between a delivery and the client's retry. A continuation writes
        // its marker on the newly created child, and by the time a retry arrives focus may have
        // moved on again (the child completed, so `move_focus_off_completed` advanced it) or
        // never moved at all (the `set_focus` below failed after the child was created). Keyed on
        // the focus topic, those retries miss the marker and re-deliver: a second child topic and
        // a second Engine execution for one user message, or — worse — the follow-up
        // `interrupt`ed into whatever unrelated topic happens to hold focus now. Keyed on the
        // session, the turn is found wherever it landed, and the topic that actually received it
        // is returned, which is exactly what `SendTurnResponse.topic_id` promises.
        if let Some(delivered) = self.already_delivered(turn_id, &topics).await? {
            return Ok(delivered); // already applied — idempotent no-op
        }

        let summaries: Vec<TopicSummary> = topics
            .iter()
            .map(|topic| TopicSummary {
                id: topic.id,
                title: topic.title.clone(),
                status: crate::status_to_str(topic.status),
                result_summary: topic.result_summary.clone(),
            })
            .collect();
        let actions = self
            .classifier
            .classify(&summaries, focus_topic_id, &content)
            .await;

        let (received, new_focus) = self
            .apply_actions(user_id, &topics, &content, actions)
            .await?;
        // Only now is the turn actually delivered, so only now is the dedup marker written. The
        // other order loses the turn for good on a transient Interrupt failure: the client's
        // documented same-turn_id retry would find the row and report success as a no-op.
        use crate::entity::message;
        let now = Utc::now();
        for topic_id in &received {
            message::ActiveModel {
                topic_id: Set(*topic_id),
                turn_id: Set(turn_id),
                content: Set(content.clone()),
                created_at: Set(now),
                ..Default::default()
            }
            .insert(&self.session.db)
            .await?;
        }

        if let Some(topic_id) = new_focus {
            self.set_focus(user_id, topic_id).await?;
        }
        Ok(received)
    }

    /// The dedup check that keeps `send_turn` idempotent, keyed on the turn across the *whole
    /// session* and never on `(focus topic, turn_id)`: focus is not stable between a delivery and
    /// the client's retry. A continuation writes its marker on the newly created child, and by the
    /// time a retry arrives focus may have moved on again (the child completed, so
    /// `move_focus_off_completed` advanced it) or never moved at all. Keyed on the focus topic,
    /// those retries miss the marker and re-deliver: a second child topic and a second Engine
    /// execution for one user message, or — worse — the follow-up `interrupt`ed into whatever
    /// unrelated topic happens to hold focus now. Keyed on the session, the turn is found wherever
    /// it landed, and every topic that received it is reported, which is exactly what
    /// `SendTurnResponse` promises.
    async fn already_delivered(
        &self,
        turn_id: Uuid,
        topics: &[topic::Model],
    ) -> Result<Option<Vec<i64>>, ChatError> {
        use crate::entity::message;
        let delivered: Vec<i64> = message::Entity::find()
            .filter(message::Column::TurnId.eq(turn_id))
            .filter(message::Column::TopicId.is_in(topics.iter().map(|t| t.id).collect::<Vec<_>>()))
            .order_by_asc(message::Column::Id)
            .all(&self.session.db)
            .await?
            .into_iter()
            .map(|row| row.topic_id)
            .collect();
        Ok((!delivered.is_empty()).then_some(delivered))
    }

    /// Applies one classification: returns `(every topic the turn reached, the topic focus should
    /// move to)`. Focus moves only to a topic this turn *created* — a new theme, or the
    /// continuation of a completed one; a turn that merely continues running topics leaves the
    /// pointer where the user put it.
    async fn apply_actions(
        self: &std::sync::Arc<Self>,
        user_id: Uuid,
        topics: &[topic::Model],
        content: &str,
        actions: Vec<Action>,
    ) -> Result<(Vec<i64>, Option<i64>), ChatError> {
        let mut received: Vec<i64> = Vec::new();
        let mut new_focus: Option<i64> = None;
        // Why a classification can end up delivering nothing: it continued a Failed or Cancelled
        // topic, which has no execution to interrupt and no answer to continue from. Held rather
        // than returned on the spot, so a second action that *does* deliver still wins.
        let mut refusal: Option<ChatError> = None;

        for action in actions {
            match action {
                Action::New { title, question } => {
                    let input_json = serde_json::json!({ "question": question }).to_string();
                    let (topic_id, _status) =
                        self.create_topic(user_id, None, title, input_json).await?;
                    received.push(topic_id);
                    new_focus.get_or_insert(topic_id);
                }
                Action::Continue { topic_id } => {
                    let Some(topic) = topics.iter().find(|t| t.id == topic_id) else {
                        continue; // the session changed under us; the other actions still stand
                    };
                    match topic.status {
                        // Spec, «Ошибки»: «Реплика в тему со статусом `Completed`: создаётся
                        // дочерняя тема от неё с этой репликой как входом — прошлое не
                        // переписывается». A completed topic's execution is gone.
                        Status::Completed => {
                            let child_id = self.continue_in_child(user_id, topic, content).await?;
                            received.push(child_id);
                            new_focus.get_or_insert(child_id);
                        }
                        Status::Running => {
                            // `execution_id` stays set forever once a topic reaches a terminal
                            // status, so its presence says nothing about whether the execution is
                            // still there — only the status does, and it says Running here.
                            let execution_id =
                                topic.execution_id.ok_or(ChatError::InvalidRequest(format!(
                                    "topic {topic_id} has no running execution to interrupt"
                                )))?;
                            let input_json = serde_json::json!({"question": content}).to_string();
                            self.engine
                                .interrupt(execution_id, &input_json)
                                .await
                                .map_err(ChatError::Engine)?;
                            received.push(topic_id);
                        }
                        // A queued topic has no execution yet: the message is recorded against it
                        // and `promote_next_queued` will start it from its original input.
                        Status::Queued => received.push(topic_id),
                        Status::Failed | Status::Cancelled => {
                            refusal.get_or_insert(ChatError::TopicNotRunning(topic_id));
                        }
                    }
                }
            }
        }

        if received.is_empty() {
            return Err(refusal.unwrap_or(ChatError::NoFocus));
        }
        Ok((received, new_focus))
    }

    /// A turn aimed at a `Completed` topic: spawn a child topic carrying the reply forward, per
    /// the spec's «создаётся дочерняя тема от неё с этой репликой как входом».
    ///
    /// It goes through `create_topic`, not a hand-rolled insert, so a continuation is subject to
    /// exactly the same rules as any other topic: the per-session concurrency cap, `Queued`
    /// admission when the cap is full, the session row lock, and the `topic_started`/
    /// `topic_queued` events the client already knows how to render.
    ///
    /// The conversation lives in the child's `question`, because that is the only state key the
    /// agent graph's prompt actually reads (`engine-core`'s `build_prompt` takes
    /// `state.question`, and the tool executor's fixed-slug path searches the same key) — a
    /// separate `context` key would be silently ignored.
    async fn continue_in_child(
        self: &std::sync::Arc<Self>,
        user_id: Uuid,
        parent: &topic::Model,
        content: &str,
    ) -> Result<i64, ChatError> {
        let question = match parent.result_summary.as_deref() {
            Some(summary) => format!("Earlier answer:\n{summary}\n\nFollow-up: {content}"),
            None => {
                // A completed topic whose final state had no `/llm/reply` — the continuation is
                // still the right thing to do, but it starts cold, so say so rather than letting
                // the thread silently lose its context.
                tracing::warn!(
                    parent_id = parent.id,
                    "completed parent topic has no result_summary; \
                     the continuation starts without the earlier answer"
                );
                content.to_owned()
            }
        };
        let title: String = content.chars().take(60).collect();
        let input_json = serde_json::json!({ "question": question }).to_string();
        let (child_id, _status) = self
            .create_topic(user_id, Some(parent.id), title, input_json)
            .await?;
        // `send_turn` writes the dedup marker and moves focus once every action has been applied.
        Ok(child_id)
    }

    /// Consumes one topic's Engine event stream to completion, updating `chat.topics` and
    /// publishing to the `EventBus` as it goes. Spawned as a background task whenever a topic
    /// starts Running — from `create_topic`, from `promote_next_queued` below, or from
    /// `recover` at startup.
    ///
    /// A stream that ends or errors *without* a terminal event (Engine restarted mid-execution,
    /// or the 3600s `StreamEvents` deadline was hit) used to strand the topic at `Running`
    /// forever — the mpsc sender in `EngineClient::stream_events`'s relay task dropped, this
    /// loop's `recv()` returned `None`, and nothing here ever called `finish_topic`. Only a chat
    /// restart's `recover()` re-attached. Now it reconnects instead: wait with backoff, call
    /// `stream_events` again — Engine's `StreamEvents` replays the whole history from version 1,
    /// so `last_seen_version` skips events this loop already handled — and keep going until a
    /// terminal event lands or the topic is no longer `Running` in the DB (reset, deleted, or
    /// finished by a `finish_topic` call this same loop already made).
    pub async fn watch_topic(self: &std::sync::Arc<Self>, topic_id: i64, execution_id: Uuid) {
        // Dedup by event id, not `version`: engine-core's `Execution::event()` currently stamps
        // every event of an execution with `version: 1` (see engine-core/src/execution.rs), so a
        // `version`-based high-water mark treats the *first* event received as having already
        // covered every later one — including the terminal event — and a reconnect (or even the
        // very first pass) would silently drop it, leaving `saw_terminal` false forever and the
        // topic stuck `Running` while Engine had long since completed it. `event.id` is a fresh
        // UUID per real event and stays stable across a replay (same DB row, same id), so it's
        // the dedup key that actually works given `version`'s current behavior — and it degrades
        // gracefully once `version` is fixed upstream, since ids stay unique either way.
        let mut seen_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut attempt: u32 = 0;
        loop {
            let mut events = match self.engine.stream_events(execution_id).await {
                Ok(events) => events,
                Err(error) => {
                    tracing::error!(topic_id, %error, "failed to open Engine event stream");
                    if !self.wait_before_reconnect(topic_id, &mut attempt).await {
                        return;
                    }
                    continue;
                }
            };

            let mut saw_terminal = false;
            while let Some(event) = events.recv().await {
                if !seen_ids.insert(event.id.clone()) {
                    continue; // already handled — a replayed event from before the reconnect
                }
                if matches!(
                    event.payload_kind.as_str(),
                    "ExecutionCompleted" | "ExecutionFailed"
                ) {
                    saw_terminal = true;
                }
                self.handle_engine_event_logged(topic_id, &event).await;
            }

            if saw_terminal {
                return; // finish_topic ran — the topic is terminal, nothing left to watch
            }
            if !self.wait_before_reconnect(topic_id, &mut attempt).await {
                return;
            }
        }
    }

    /// Between one `stream_events` attempt and the next: exits quietly (returns `false`) once the
    /// topic is no longer `Running` — a reset or delete out from under this watcher, or a status
    /// this same call already moved off `Running` some other way — so a dead topic doesn't get
    /// reconnected to forever. Otherwise sleeps out an exponential backoff (capped at
    /// `reconnect_max_delay`, doubling from `reconnect_base_delay`) and returns `true`.
    async fn wait_before_reconnect(&self, topic_id: i64, attempt: &mut u32) -> bool {
        match topic::Entity::find_by_id(topic_id)
            .one(&self.session.db)
            .await
        {
            Ok(Some(row)) if row.status == Status::Running => {}
            Ok(_) => return false, // no longer Running (or gone) — stop watching quietly
            Err(error) => {
                tracing::error!(topic_id, %error, "failed to check topic status before reconnecting");
                return false;
            }
        }

        *attempt += 1;
        let delay = self
            .reconnect_base_delay
            .saturating_mul(1u32.checked_shl(*attempt - 1).unwrap_or(u32::MAX))
            .min(self.reconnect_max_delay);
        tracing::warn!(
            topic_id,
            attempt = *attempt,
            delay_ms = delay.as_millis() as u64,
            "Engine event stream ended without a terminal event; reconnecting"
        );
        tokio::time::sleep(delay).await;
        true
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
                let session_id = self.session_of_topic(topic_id).await?;
                self.publish(
                    session_id,
                    ChatEvent {
                        topic_id: topic_id.to_string(),
                        kind: "topic_progress".to_owned(),
                        payload_json: event.payload_json.clone(),
                        occurred_at: event.occurred_at.clone(),
                        ..Default::default()
                    },
                )
                .await;
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

        self.publish_terminal_events(&row, status, summary.as_deref(), focus_change)
            .await;

        self.promote_next_queued(row.session_id).await
    }

    /// The three events a terminal topic produces, in the order a client has to see them: the
    /// lifecycle event, the notification carrying the answer («уведомление — это событие, которое
    /// клиент отображает в чате»), and — only when the finished topic held focus — where focus
    /// went.
    async fn publish_terminal_events(
        &self,
        row: &topic::Model,
        status: Status,
        summary: Option<&str>,
        focus_change: Option<(Option<i64>, i64)>,
    ) {
        let completed = status == Status::Completed;
        let session_id = row.session_id;
        let occurred_at = row.updated_at.to_rfc3339();
        self.publish(
            session_id,
            ChatEvent {
                topic_id: row.id.to_string(),
                kind: if completed {
                    "topic_completed"
                } else {
                    "topic_failed"
                }
                .to_owned(),
                payload_json: serde_json::json!({"summary": summary}).to_string(),
                occurred_at: occurred_at.clone(),
                ..Default::default()
            },
        )
        .await;
        self.publish(
            session_id,
            ChatEvent {
                topic_id: row.id.to_string(),
                kind: "notification".to_owned(),
                payload_json: serde_json::json!({
                    "kind": if completed { "completed" } else { "failed" },
                    "text": summary,
                })
                .to_string(),
                occurred_at,
                ..Default::default()
            },
        )
        .await;

        if let Some((next, from)) = focus_change {
            self.publish(
                session_id,
                ChatEvent {
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
                },
            )
            .await;
        }
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
        let execution_id = self
            .engine
            .start_execution("agent", &next.input_json)
            .await
            .map_err(ChatError::Engine)?;
        let mut active: topic::ActiveModel = next.clone().into();
        active.status = Set(Status::Running);
        active.execution_id = Set(Some(execution_id));
        active.updated_at = Set(Utc::now());
        active.update(&txn).await?;
        txn.commit().await?;

        self.publish(
            session_id,
            ChatEvent {
                topic_id: next.id.to_string(),
                kind: "topic_started".to_owned(),
                occurred_at: Utc::now().to_rfc3339(),
                ..Default::default()
            },
        )
        .await;
        self.spawn_watch(next.id, execution_id);
        Ok(())
    }

    /// "Start over": wipes the user's session — every message, every topic, and the session row
    /// itself — under the same session row lock every other focus/status-changing path uses, so
    /// this can't race a concurrent `create_topic`/`finish_topic`/`promote_next_queued` for the
    /// same session.
    ///
    /// Still-`Running` topics get a best-effort `Interrupt` first so their Engine executions stop
    /// making progress; the interrupt result is ignored (the topic row is about to be deleted
    /// either way) and any in-flight `watch_topic` task hits `TopicNotFound` on its next event —
    /// `handle_engine_event_logged` only logs that, never panics. `get_or_create_session` builds
    /// a brand-new session row on the next call, exactly as if this were a first-time user.
    pub async fn reset_session(&self, user_id: Uuid) -> Result<(), ChatError> {
        let session = self.session.get_or_create_session(user_id).await?;

        let txn = self.session.db.begin().await?;
        let locked = lock_session(&txn, session.id).await?;

        let topics = topic::Entity::find()
            .filter(topic::Column::SessionId.eq(locked.id))
            .all(&txn)
            .await?;

        for topic in &topics {
            if topic.status == Status::Running
                && let Some(execution_id) = topic.execution_id
            {
                let _ = self.engine.interrupt(execution_id, "{}").await;
            }
        }

        let topic_ids: Vec<i64> = topics.iter().map(|t| t.id).collect();
        if !topic_ids.is_empty() {
            use crate::entity::message;
            message::Entity::delete_many()
                .filter(message::Column::TopicId.is_in(topic_ids.clone()))
                .exec(&txn)
                .await?;
            topic::Entity::delete_many()
                .filter(topic::Column::Id.is_in(topic_ids))
                .exec(&txn)
                .await?;
        }
        // The session's event log goes with it: a reset means "none of this happened", and the
        // replay a reconnecting client gets must not resurrect topics that no longer exist. This
        // is a per-session domain delete over one user's rows, not the log's retention path
        // (which is DROP PARTITION — see `event_log`).
        event::Entity::delete_many()
            .filter(event::Column::SessionId.eq(locked.id))
            .exec(&txn)
            .await?;
        session::Entity::delete_by_id(locked.id).exec(&txn).await?;
        txn.commit().await?;

        // Published, deliberately not persisted: the row would land in the log of a session that
        // was just deleted, and the next `get_or_create_session` mints a new session id anyway.
        self.events.publish(ChatEvent {
            topic_id: String::new(),
            kind: "session_reset".to_owned(),
            occurred_at: Utc::now().to_rfc3339(),
            session_id: session.id.to_string(),
            ..Default::default()
        });
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
    use common::proto::llm_router::v1::{
        CompleteRequest, CompleteResponse, DescribeTiersRequest, DescribeTiersResponse,
        LlmRouterService,
    };
    use connectrpc::{
        RequestContext, Response, Router as ConnectRouter, ServiceRequest, ServiceResult,
        ServiceStream,
    };
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

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
        /// execution_id (string) -> per-call scripts, one `Vec<ExecutionEvent>` per
        /// `stream_events` call in order — the Nth call to `stream_events` for that execution_id
        /// gets `scripts[N]` (clamped to the last entry once exhausted). Lets a test make the
        /// *first* call end without a terminal event (reproducing a dropped Engine connection)
        /// and a *later* call deliver the terminal event, the way Engine's real replay-from-
        /// version-1 behavior would. Takes priority over `completions` when both are armed.
        scripted_streams: Mutex<HashMap<String, Vec<Vec<ExecutionEvent>>>>,
        /// execution_id (string) -> how many times `stream_events` has been called for it —
        /// what the reconnect tests assert on.
        stream_calls: Mutex<HashMap<String, u32>>,
    }

    impl FakeEngine {
        /// Registers the sequence of per-call event scripts `stream_events` replays for
        /// `execution_id` — see `scripted_streams`' doc.
        fn arm_scripted_stream(&self, execution_id: Uuid, calls: Vec<Vec<ExecutionEvent>>) {
            self.scripted_streams
                .lock()
                .expect("lock")
                .insert(execution_id.to_string(), calls);
        }

        fn stream_call_count(&self, execution_id: Uuid) -> u32 {
            self.stream_calls
                .lock()
                .expect("lock")
                .get(&execution_id.to_string())
                .copied()
                .unwrap_or(0)
        }
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
            let Some(execution_id) = owned.execution_id else {
                return Response::stream_ok(futures::stream::empty());
            };

            let call_index = {
                let mut calls = self.stream_calls.lock().expect("lock");
                let entry = calls.entry(execution_id.clone()).or_insert(0);
                let index = *entry;
                *entry += 1;
                index
            };

            if let Some(scripts) = self
                .scripted_streams
                .lock()
                .expect("lock")
                .get(&execution_id)
            {
                let index = (call_index as usize).min(scripts.len().saturating_sub(1));
                let events = scripts.get(index).cloned().unwrap_or_default();
                return Response::stream_ok(futures::stream::iter(
                    events.into_iter().map(Ok).collect::<Vec<_>>(),
                ));
            }

            let event = self
                .completions
                .lock()
                .expect("lock")
                .get(&execution_id)
                .cloned();
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

    /// The llm-router stand-in for the topic classifier: hands back whatever JSON a test armed
    /// it with (or fails, when the test is about the fallback path). Same shape as `FakeEngine`.
    #[derive(Default)]
    struct FakeLlmRouter {
        /// The `content` of the next `Complete` response. `None` = answer with an error, which
        /// is what drives `TopicClassifier`'s fallback.
        answer: Mutex<Option<String>>,
        prompts: Mutex<Vec<String>>,
    }

    impl FakeLlmRouter {
        fn answer_with(&self, content: &str) {
            *self.answer.lock().expect("lock") = Some(content.to_owned());
        }
    }

    #[allow(refining_impl_trait)]
    impl LlmRouterService for FakeLlmRouter {
        async fn complete(
            &self,
            _ctx: RequestContext,
            request: ServiceRequest<'_, CompleteRequest>,
        ) -> ServiceResult<CompleteResponse> {
            let owned = request.to_owned_message();
            self.prompts.lock().expect("lock").push(owned.user_prompt);
            match self.answer.lock().expect("lock").clone() {
                Some(content) => Response::ok(CompleteResponse {
                    content,
                    ..Default::default()
                }),
                None => Err(connectrpc::ConnectError::unavailable(
                    "configured fake llm-router failure",
                )),
            }
        }
        async fn describe_tiers(
            &self,
            _ctx: RequestContext,
            _request: ServiceRequest<'_, DescribeTiersRequest>,
        ) -> ServiceResult<DescribeTiersResponse> {
            Response::ok(DescribeTiersResponse::default())
        }
    }

    async fn serve(connect: ConnectRouter) -> String {
        let app = axum::Router::new().fallback_service(connect.into_axum_service());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("addr");
        tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
        format!("http://{address}")
    }

    async fn manager_with() -> (crate::test_db::TestDb, Arc<TopicManager>, Arc<FakeEngine>) {
        let (test, manager, engine, _router) = manager_with_router().await;
        (test, manager, engine)
    }

    async fn manager_with_router() -> (
        crate::test_db::TestDb,
        Arc<TopicManager>,
        Arc<FakeEngine>,
        Arc<FakeLlmRouter>,
    ) {
        let test = crate::test_db::start().await;
        let fake = Arc::new(FakeEngine::default());
        let router = Arc::new(FakeLlmRouter::default());
        let engine_url = serve(ConnectRouter::new().add_service(Arc::clone(&fake))).await;
        let router_url = serve(ConnectRouter::new().add_service(Arc::clone(&router))).await;
        let manager = Arc::new(
            TopicManager::new(test.db.clone(), &engine_url, &router_url)
                .expect("manager")
                // A reconnect test would otherwise wait out a real 1s..30s backoff.
                .with_reconnect_delays(Duration::from_millis(1), Duration::from_millis(20)),
        );
        (test, manager, fake, router)
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
        assert_eq!(routed, vec![topic_id]);

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
        let queued_input_json = r#"{"question":"overflow question"}"#;
        let (queued_id, status) = manager
            .create_topic(user_id, None, "Overflow".into(), queued_input_json.into())
            .await
            .expect("create");
        assert_eq!(status, Status::Queued);

        let finishing_id = running_ids[0];
        // The real Engine's envelope shape, verbatim (stream.rs::row_to_proto sends the whole
        // externally-tagged payload, not the inner body).
        let execution_id = arm_terminal_event(
            &manager,
            &fake,
            finishing_id,
            "ExecutionCompleted",
            r#"{"ExecutionCompleted":{"final_state":{"llm":{"reply":"done"}}}}"#,
        )
        .await;
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

        let start_executions = fake.start_executions.lock().expect("lock");
        assert_eq!(
            start_executions.len(),
            start_executions_before + 1,
            "promotion must call start_execution on the Engine for the promoted topic"
        );
        assert_eq!(
            start_executions.last().expect("promotion call").input_json,
            queued_input_json
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

    /// `send_turn` returns every topic a turn reached; most tests route to exactly one and want
    /// that id, with the count itself asserted.
    fn one_topic(topic_ids: Vec<i64>) -> i64 {
        assert_eq!(topic_ids.len(), 1, "expected one topic: {topic_ids:?}");
        topic_ids[0]
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
    ///
    /// `Failed` (not `Completed`) is the case that stays refused: the spec's continuation rule
    /// covers `Completed` only, and a failed topic has no answer to continue from.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_turn_for_a_failed_focus_topic_is_refused_rather_than_interrupted() {
        let (_test, manager, fake) = manager_with().await;
        let user_id = Uuid::new_v4();
        let (topic_id, _) = manager
            .create_topic(user_id, None, "First".into(), "{}".into())
            .await
            .expect("create");
        set_status(&manager, topic_id, Status::Failed).await;

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

    const PARENT_ANSWER: &str = "It is a coding agent.";
    const FOLLOW_UP: &str = "Which IDEs does it integrate with?";

    /// Leaves the session with one `Completed` topic that answered `PARENT_ANSWER`, focused —
    /// the exact state the user is in when they type a follow-up. Returns
    /// `(user_id, parent_id, start_execution calls so far)`.
    async fn session_with_a_completed_focus_topic(
        manager: &Arc<TopicManager>,
        fake: &FakeEngine,
    ) -> (Uuid, i64, usize) {
        let user_id = Uuid::new_v4();
        let (parent_id, _) = manager
            .create_topic(user_id, None, "What is Claude Code?".into(), "{}".into())
            .await
            .expect("create");
        let execution_id = arm_terminal_event(
            manager,
            fake,
            parent_id,
            "ExecutionCompleted",
            &format!(
                r#"{{"ExecutionCompleted":{{"final_state":{{"llm":{{"reply":"{PARENT_ANSWER}"}}}}}}}}"#
            ),
        )
        .await;
        manager.watch_topic(parent_id, execution_id).await;
        // The parent completed with nothing else unfinished, so focus was cleared; put the user
        // back on the topic they were reading, which is where a follow-up is actually typed.
        manager
            .set_focus(user_id, parent_id)
            .await
            .expect("set_focus");
        let starts = fake.start_executions.lock().expect("lock").len();
        (user_id, parent_id, starts)
    }

    /// The user's report: «чаты при создании топика сразу закрываются» — a topic finishes in
    /// seconds and every further message was refused, so the chat looked dead on arrival. Spec,
    /// «Ошибки»: a reply to a `Completed` topic creates a child topic from it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_turn_for_a_completed_focus_topic_starts_a_child_that_carries_the_answer_forward() {
        let (_test, manager, fake) = manager_with().await;
        let (user_id, parent_id, starts_before) =
            session_with_a_completed_focus_topic(&manager, &fake).await;

        let turn_id = Uuid::new_v4();
        let child_id = one_topic(
            manager
                .send_turn(user_id, turn_id, FOLLOW_UP.into())
                .await
                .expect("a completed topic must accept a follow-up"),
        );

        assert_ne!(child_id, parent_id, "the follow-up gets its own topic");
        let child = topic::Entity::find_by_id(child_id)
            .one(&manager.session.db)
            .await
            .expect("query")
            .expect("child topic");
        assert_eq!(child.parent_id, Some(parent_id));
        assert_eq!(child.status, Status::Running);
        assert_eq!(child.title, FOLLOW_UP);
        assert_eq!(
            focus_of(&manager, user_id).await,
            Some(child_id),
            "the user is talking to the continuation now"
        );

        let starts = fake.start_executions.lock().expect("lock");
        assert_eq!(
            starts.len(),
            starts_before + 1,
            "exactly one new Engine execution for the child topic"
        );
        let input = &starts.last().expect("child start").input_json;
        let state: serde_json::Value = serde_json::from_str(input).expect("input is json");
        // It has to land under `question`: that is the only state key the agent graph's prompt
        // reads, so a follow-up parked anywhere else would be silently ignored.
        let question = state
            .get("question")
            .and_then(serde_json::Value::as_str)
            .expect("the child's input carries a `question`");
        assert!(
            question.contains(PARENT_ANSWER),
            "the child's question must carry the parent's answer: {question}"
        );
        assert!(
            question.contains(FOLLOW_UP),
            "the child's question must carry the new turn: {question}"
        );
    }

    /// The review finding: the dedup lookup was keyed on `(focus topic, turn_id)`, but a
    /// continuation writes its marker on the *child*. Focus does not stand still between a
    /// delivery and the client's retry — here the child completes first, so focus moves on to a
    /// Running sibling. Keyed on the focus topic, the retry missed the marker and `interrupt`ed
    /// the follow-up into that unrelated topic; keyed on the session, it is recognised as already
    /// delivered and still reports the child that received it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_retry_is_still_a_no_op_after_focus_has_moved_off_the_child() {
        let (_test, manager, fake) = manager_with().await;
        let (user_id, _parent_id, _starts) =
            session_with_a_completed_focus_topic(&manager, &fake).await;
        // A background topic that stays Running — focus lands here when the child completes.
        let (sibling_id, status) = manager
            .create_topic(user_id, None, "Background".into(), "{}".into())
            .await
            .expect("create sibling");
        assert_eq!(status, Status::Running);

        let turn_id = Uuid::new_v4();
        let child_id = one_topic(
            manager
                .send_turn(user_id, turn_id, FOLLOW_UP.into())
                .await
                .expect("send_turn"),
        );
        let starts_after_child = fake.start_executions.lock().expect("lock").len();

        // The child completes, so focus advances to the still-Running sibling.
        let execution_id = arm_terminal_event(
            &manager,
            &fake,
            child_id,
            "ExecutionCompleted",
            r#"{"ExecutionCompleted":{"final_state":{"llm":{"reply":"VS Code and JetBrains."}}}}"#,
        )
        .await;
        manager.watch_topic(child_id, execution_id).await;
        assert_eq!(
            focus_of(&manager, user_id).await,
            Some(sibling_id),
            "focus must have moved off the completed child for this test to mean anything"
        );

        let repeated = one_topic(
            manager
                .send_turn(user_id, turn_id, FOLLOW_UP.into())
                .await
                .expect("the retry must be recognised as already delivered"),
        );

        assert_eq!(
            repeated, child_id,
            "the retry must report the topic that actually received the turn, not the new focus"
        );
        assert!(
            fake.interrupts.lock().expect("lock").is_empty(),
            "the follow-up must not be interrupted into the unrelated background topic"
        );
        assert_eq!(
            fake.start_executions.lock().expect("lock").len(),
            starts_after_child,
            "and no second continuation may be started"
        );
    }

    /// The turn_id contract holds across the continuation path too: a client retry must not
    /// spawn a second child topic (and a second Engine execution) for one user message.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_repeated_turn_id_does_not_spawn_a_second_continuation() {
        let (_test, manager, fake) = manager_with().await;
        let (user_id, _parent_id, starts_before) =
            session_with_a_completed_focus_topic(&manager, &fake).await;
        let turn_id = Uuid::new_v4();
        let child_id = one_topic(
            manager
                .send_turn(user_id, turn_id, FOLLOW_UP.into())
                .await
                .expect("send_turn"),
        );

        let repeated = one_topic(
            manager
                .send_turn(user_id, turn_id, FOLLOW_UP.into())
                .await
                .expect("the repeat must succeed as a no-op"),
        );
        assert_eq!(repeated, child_id);
        assert_eq!(
            fake.start_executions.lock().expect("lock").len(),
            starts_before + 1,
            "a repeated turn_id must not spawn a second continuation"
        );
        use crate::entity::message;
        let stored = message::Entity::find()
            .filter(message::Column::TopicId.eq(child_id))
            .filter(message::Column::TurnId.eq(turn_id))
            .all(&manager.session.db)
            .await
            .expect("query messages");
        assert_eq!(stored.len(), 1, "one message row on the child, not two");
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

        assert_eq!(routed, vec![topic_id]);
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

    /// "Start over": two topics, then `ResetSession` — the session view must come back empty
    /// with no focus, and a `CreateTopic` afterwards must become focus again exactly like a
    /// brand-new user's first topic (see
    /// `the_first_topic_in_a_session_starts_running_and_becomes_focus`).
    #[tokio::test(flavor = "multi_thread")]
    async fn reset_session_clears_everything_and_a_fresh_topic_becomes_focus_again() {
        let (_test, manager, _fake) = manager_with().await;
        let user_id = Uuid::new_v4();
        manager
            .create_topic(user_id, None, "First".into(), "{}".into())
            .await
            .expect("create");
        manager
            .create_topic(user_id, None, "Second".into(), "{}".into())
            .await
            .expect("create");

        manager.reset_session(user_id).await.expect("reset");

        let (focus, topics) = manager
            .session
            .get_session_view(user_id)
            .await
            .expect("view");
        assert_eq!(focus, None, "reset must clear focus");
        assert!(topics.is_empty(), "reset must clear every topic");

        let (fresh_id, status) = manager
            .create_topic(user_id, None, "Fresh".into(), "{}".into())
            .await
            .expect("create after reset");
        assert_eq!(status, Status::Running);
        let (focus, _) = manager
            .session
            .get_session_view(user_id)
            .await
            .expect("view");
        assert_eq!(
            focus,
            Some(fresh_id),
            "the first topic of the new session becomes focus again"
        );
    }

    /// The product decision this change implements: the user never creates a topic by hand, so
    /// the very first message of a session has to become one — title, question and a started
    /// Engine execution — with no `CreateTopic` call anywhere.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_first_message_in_an_empty_session_becomes_a_started_topic() {
        let (_test, manager, fake, router) = manager_with_router().await;
        let user_id = Uuid::new_v4();
        router.answer_with(
            r#"{"actions":[{"kind":"new","title":"Claude Code","question":"What is Claude Code?"}]}"#,
        );

        let topic_id = one_topic(
            manager
                .send_turn(user_id, Uuid::new_v4(), "What is Claude Code?".into())
                .await
                .expect("send_turn"),
        );

        let topic = topic::Entity::find_by_id(topic_id)
            .one(&manager.session.db)
            .await
            .expect("query")
            .expect("topic");
        assert_eq!(topic.title, "Claude Code");
        assert_eq!(topic.status, Status::Running);
        assert_eq!(focus_of(&manager, user_id).await, Some(topic_id));
        let starts = fake.start_executions.lock().expect("lock");
        assert_eq!(starts.len(), 1, "the message started exactly one execution");
        let state: serde_json::Value =
            serde_json::from_str(&starts[0].input_json).expect("input is json");
        assert_eq!(
            state.get("question").and_then(serde_json::Value::as_str),
            Some("What is Claude Code?"),
            "the classifier's self-contained question is what the worker is given"
        );
    }

    /// «Первое сообщение содержит несколько тем → несколько корневых тем, одна в фокусе,
    /// остальные выполняются параллельно» — and `SendTurnResponse` has to name both, which is
    /// why `topic_ids` exists.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_message_naming_two_themes_opens_two_topics_and_focuses_the_first() {
        let (_test, manager, fake, router) = manager_with_router().await;
        let user_id = Uuid::new_v4();
        router.answer_with(
            r#"{"actions":[
                {"kind":"new","title":"Claude Code","question":"What is Claude Code?"},
                {"kind":"new","title":"Academy courses","question":"Which Claude Academy courses exist?"}
            ]}"#,
        );

        let topic_ids = manager
            .send_turn(
                user_id,
                Uuid::new_v4(),
                "What is Claude Code? And which Academy courses exist?".into(),
            )
            .await
            .expect("send_turn");

        assert_eq!(topic_ids.len(), 2, "one topic per theme: {topic_ids:?}");
        assert_eq!(
            focus_of(&manager, user_id).await,
            Some(topic_ids[0]),
            "focus goes to the first new topic"
        );
        assert_eq!(
            fake.start_executions.lock().expect("lock").len(),
            2,
            "both topics run in parallel"
        );
        // The turn is recorded against every topic that received it.
        use crate::entity::message;
        for topic_id in &topic_ids {
            let stored = message::Entity::find()
                .filter(message::Column::TopicId.eq(*topic_id))
                .all(&manager.session.db)
                .await
                .expect("query");
            assert_eq!(stored.len(), 1, "topic {topic_id} holds the user's message");
        }
    }

    /// The common case, and the one that must not regress into topic sprawl: a follow-up the
    /// classifier calls a continuation of the running focus is `interrupt`ed into it, with no new
    /// topic and no new execution.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_continue_on_the_running_focus_interrupts_it_and_creates_no_topic() {
        let (_test, manager, fake, router) = manager_with_router().await;
        let user_id = Uuid::new_v4();
        let (topic_id, _) = manager
            .create_topic(user_id, None, "Claude Code".into(), "{}".into())
            .await
            .expect("create");
        let starts_before = fake.start_executions.lock().expect("lock").len();
        router.answer_with(&format!(
            r#"{{"actions":[{{"kind":"continue","topic_id":{topic_id}}}]}}"#
        ));

        let routed = manager
            .send_turn(user_id, Uuid::new_v4(), "and which IDEs?".into())
            .await
            .expect("send_turn");

        assert_eq!(routed, vec![topic_id]);
        assert_eq!(fake.interrupts.lock().expect("lock").len(), 1);
        assert_eq!(
            fake.start_executions.lock().expect("lock").len(),
            starts_before,
            "a continuation must not start a second execution"
        );
    }

    /// Classification is advisory: an llm-router that is down (or answers nonsense) must never
    /// cost the user their turn. The fallback continues the focused topic, which is what the
    /// service did before the classifier existed.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_classifier_failure_falls_back_to_continuing_the_focused_topic() {
        let (_test, manager, fake, router) = manager_with_router().await;
        let user_id = Uuid::new_v4();
        let (topic_id, _) = manager
            .create_topic(user_id, None, "Claude Code".into(), "{}".into())
            .await
            .expect("create");
        // `answer` left unset — every Complete call fails.
        assert!(router.answer.lock().expect("lock").is_none());

        let routed = manager
            .send_turn(user_id, Uuid::new_v4(), "and which IDEs?".into())
            .await
            .expect("a classifier outage must not fail the turn");

        assert_eq!(routed, vec![topic_id]);
        assert_eq!(fake.interrupts.lock().expect("lock").len(), 1);
    }

    /// One session-wide transcript needs the user's own turns back from the server, not just
    /// topic titles — `GetSession` renders from these rows after a reload.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_session_view_returns_each_topic_s_messages_in_order() {
        let (_test, manager, _fake, router) = manager_with_router().await;
        let user_id = Uuid::new_v4();
        router.answer_with(
            r#"{"actions":[{"kind":"new","title":"Claude Code","question":"What is Claude Code?"}]}"#,
        );
        let first_turn_id = Uuid::new_v4();
        let topic_id = one_topic(
            manager
                .send_turn(user_id, first_turn_id, "What is Claude Code?".into())
                .await
                .expect("send_turn"),
        );
        router.answer_with(&format!(
            r#"{{"actions":[{{"kind":"continue","topic_id":{topic_id}}}]}}"#
        ));
        let second_turn_id = Uuid::new_v4();
        manager
            .send_turn(user_id, second_turn_id, "and which IDEs?".into())
            .await
            .expect("send_turn");

        let (_focus, topics) = manager
            .session
            .get_session_view(user_id)
            .await
            .expect("view");
        let messages = manager
            .session
            .messages_by_topic(&topics.iter().map(|t| t.id).collect::<Vec<_>>())
            .await
            .expect("messages");

        let contents: Vec<&str> = messages[&topic_id]
            .iter()
            .map(|m| m.content.as_str())
            .collect();
        assert_eq!(contents, vec!["What is Claude Code?", "and which IDEs?"]);
        // The client needs `turn_id` back on each message to collapse the same turn's rows
        // across every topic the classifier routed it to (see chat.proto's Message.turn_id).
        let turn_ids: Vec<Uuid> = messages[&topic_id].iter().map(|m| m.turn_id).collect();
        assert_eq!(turn_ids, vec![first_turn_id, second_turn_id]);
    }

    /// The debug panel is session-wide *and* survives a reload, which it can only do if every
    /// published event is also a row — and if the replay comes back in the order it happened.
    #[tokio::test(flavor = "multi_thread")]
    async fn events_are_persisted_and_replayed_in_order_then_cleared_by_a_reset() {
        let (_test, manager, _fake, _router) = manager_with_router().await;
        let user_id = Uuid::new_v4();
        let (first_id, _) = manager
            .create_topic(user_id, None, "First".into(), "{}".into())
            .await
            .expect("create");
        let (second_id, _) = manager
            .create_topic(user_id, None, "Second".into(), "{}".into())
            .await
            .expect("create");
        manager
            .set_focus(user_id, second_id)
            .await
            .expect("set_focus");
        let session_id = manager
            .session
            .get_or_create_session(user_id)
            .await
            .expect("session")
            .id;

        let replay = manager.stored_events(session_id).await.expect("replay");

        let kinds: Vec<&str> = replay.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(
            kinds,
            vec![
                "topic_created",
                "topic_started",
                "topic_created",
                "topic_started",
                "focus_changed",
            ],
            "the replay is the session's history in the order it happened"
        );
        assert_eq!(replay[0].topic_id, first_id.to_string());
        assert_eq!(replay[4].topic_id, second_id.to_string());
        assert!(
            replay
                .iter()
                .all(|e| e.session_id == session_id.to_string()),
            "every replayed event names its session, which is what the live tail filters on"
        );

        manager.reset_session(user_id).await.expect("reset");

        assert!(
            manager
                .stored_events(session_id)
                .await
                .expect("replay")
                .is_empty(),
            "a reset must leave no events behind to resurrect deleted topics"
        );
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

    /// A `NodeCompleted` event at a given version — the reconnect tests' stand-in for
    /// in-progress `topic_progress` events.
    fn node_completed_event(execution_id: Uuid, version: i32) -> ExecutionEvent {
        ExecutionEvent {
            id: format!("e{version}"),
            execution_id: execution_id.to_string(),
            version,
            payload_kind: "NodeCompleted".to_owned(),
            payload_json: r#"{"node":"search"}"#.to_owned(),
            ..Default::default()
        }
    }

    /// An `ExecutionCompleted` event at a given version, carrying `reply` as the final answer —
    /// the real Engine envelope shape (see `arm_terminal_event`'s doc).
    fn execution_completed_event(execution_id: Uuid, version: i32, reply: &str) -> ExecutionEvent {
        ExecutionEvent {
            id: format!("e{version}"),
            execution_id: execution_id.to_string(),
            version,
            payload_kind: "ExecutionCompleted".to_owned(),
            payload_json: format!(
                r#"{{"ExecutionCompleted":{{"final_state":{{"llm":{{"reply":"{reply}"}}}}}}}}"#
            ),
            ..Default::default()
        }
    }

    /// Inserts a `Running` topic row directly — session, title, a freshly-minted `execution_id`
    /// — without going through `create_topic`. The reconnect tests need exactly *one*
    /// `watch_topic` consumer racing `stream_events`: `create_topic` would call `start_execution`
    /// and `spawn_watch` a second, background watcher for the same topic, which would also start
    /// reconnect-polling the fake and make the `stream_call_count` assertions non-deterministic.
    async fn insert_running_topic(manager: &Arc<TopicManager>, user_id: Uuid) -> (i64, Uuid) {
        let session = manager
            .session
            .get_or_create_session(user_id)
            .await
            .expect("session");
        let execution_id = Uuid::new_v4();
        let now = Utc::now();
        let row = topic::ActiveModel {
            session_id: Set(session.id),
            parent_id: Set(None),
            title: Set("First".to_owned()),
            status: Set(Status::Running),
            execution_id: Set(Some(execution_id)),
            input_json: Set("{}".to_owned()),
            result_summary: Set(None),
            artifact_ids: Set(serde_json::json!([])),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(&manager.session.db)
        .await
        .expect("insert topic");
        (row.id, execution_id)
    }

    /// The live bug this change fixes: Engine restarted mid-execution, the event stream ended
    /// (no `ExecutionCompleted`/`ExecutionFailed`), and `watch_topic` used to just return —
    /// stranding the topic at `Running` forever. Now it reconnects: the fake's first
    /// `stream_events` call for this execution ends the stream early (an empty script, standing
    /// in for the dropped connection), and its second call delivers the terminal event, exactly
    /// as Engine's real `StreamEvents` would on a fresh subscribe after a restart (it replays
    /// from version 1).
    #[tokio::test(flavor = "multi_thread")]
    async fn watch_topic_reconnects_after_a_stream_that_ends_without_a_terminal_event() {
        let (_test, manager, fake) = manager_with().await;
        let user_id = Uuid::new_v4();
        let (topic_id, execution_id) = insert_running_topic(&manager, user_id).await;

        fake.arm_scripted_stream(
            execution_id,
            vec![
                vec![], // first call: stream ends immediately, no terminal event
                vec![execution_completed_event(execution_id, 1, "done")], // the reconnect
            ],
        );

        manager.watch_topic(topic_id, execution_id).await;

        let finished = topic::Entity::find_by_id(topic_id)
            .one(&manager.session.db)
            .await
            .expect("query")
            .expect("topic");
        assert_eq!(
            finished.status,
            Status::Completed,
            "the reconnect must let the topic reach its terminal status"
        );
        assert_eq!(
            fake.stream_call_count(execution_id),
            2,
            "stream_events must be called again after the first stream ended without a terminal event"
        );
    }

    /// The live regression this change fixes: engine-core's real `Execution::event()` stamps
    /// *every* event of an execution with `version: 1` (see engine-core/src/execution.rs), unlike
    /// this test file's other fixtures which increment `version` per event. With a
    /// `version`-based high-water mark, the first event received (version 1) makes every
    /// following event — including the terminal one, also version 1 — look "already handled" and
    /// get skipped, so `saw_terminal` never becomes true and `watch_topic` reconnects forever
    /// while the topic stays `Running` even though Engine's execution completed. Dedup must be by
    /// event id instead, so a terminal event at the same `version` as an earlier progress event
    /// still gets processed and `finish_topic` runs on the very first pass — no reconnect.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_terminal_event_sharing_its_version_with_an_earlier_event_still_finishes_the_topic() {
        let (_test, manager, fake) = manager_with().await;
        let user_id = Uuid::new_v4();
        let (topic_id, execution_id) = insert_running_topic(&manager, user_id).await;

        // Every event at version 1, matching engine-core's real (buggy) `Execution::event()` —
        // only the ids differ, exactly as real Uuid-per-event ids would.
        fake.arm_scripted_stream(
            execution_id,
            vec![vec![
                ExecutionEvent {
                    id: "e-progress".to_owned(),
                    execution_id: execution_id.to_string(),
                    version: 1,
                    payload_kind: "NodeCompleted".to_owned(),
                    payload_json: r#"{"node":"search"}"#.to_owned(),
                    ..Default::default()
                },
                ExecutionEvent {
                    id: "e-terminal".to_owned(),
                    execution_id: execution_id.to_string(),
                    version: 1,
                    payload_kind: "ExecutionCompleted".to_owned(),
                    payload_json:
                        r#"{"ExecutionCompleted":{"final_state":{"llm":{"reply":"done"}}}}"#
                            .to_owned(),
                    ..Default::default()
                },
            ]],
        );

        manager.watch_topic(topic_id, execution_id).await;

        let finished = topic::Entity::find_by_id(topic_id)
            .one(&manager.session.db)
            .await
            .expect("query")
            .expect("topic");
        assert_eq!(
            finished.status,
            Status::Completed,
            "a terminal event must finish the topic even when it shares its version with an \
             earlier event"
        );
        assert_eq!(
            fake.stream_call_count(execution_id),
            1,
            "the terminal event was in the first stream — there must be no reconnect"
        );
    }

    /// Engine's `StreamEvents` replays the whole history from version 1 on every subscribe, so a
    /// reconnect re-delivers events `watch_topic` already handled before the stream broke. Those
    /// must be skipped by version, not re-applied — otherwise a reconnect would double-publish
    /// every `topic_progress` event the topic had already emitted.
    #[tokio::test(flavor = "multi_thread")]
    async fn replayed_events_are_not_double_handled_after_a_reconnect() {
        let (_test, manager, fake) = manager_with().await;
        let user_id = Uuid::new_v4();
        let (topic_id, execution_id) = insert_running_topic(&manager, user_id).await;
        let session_id = manager
            .session
            .get_or_create_session(user_id)
            .await
            .expect("session")
            .id;

        fake.arm_scripted_stream(
            execution_id,
            vec![
                // First call: two progress events, then the stream ends (no terminal event).
                vec![
                    node_completed_event(execution_id, 1),
                    node_completed_event(execution_id, 2),
                ],
                // Reconnect: Engine's full replay from version 1 — versions 1 and 2 again, plus
                // the completion this time.
                vec![
                    node_completed_event(execution_id, 1),
                    node_completed_event(execution_id, 2),
                    execution_completed_event(execution_id, 3, "done"),
                ],
            ],
        );

        manager.watch_topic(topic_id, execution_id).await;

        assert_eq!(
            fake.stream_call_count(execution_id),
            2,
            "the broken first stream must trigger exactly one reconnect"
        );
        let finished = topic::Entity::find_by_id(topic_id)
            .one(&manager.session.db)
            .await
            .expect("query")
            .expect("topic");
        assert_eq!(finished.status, Status::Completed);

        let replay = manager.stored_events(session_id).await.expect("replay");
        let progress_count = replay.iter().filter(|e| e.kind == "topic_progress").count();
        assert_eq!(
            progress_count, 2,
            "versions 1 and 2 replayed by the reconnect must not be published a second time"
        );
    }
}
