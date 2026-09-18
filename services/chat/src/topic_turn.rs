// Routing one user turn onto topics, once per turn_id however often the client retries.

use std::sync::Arc;

use chrono::Utc;
use common::execution_input::ExecutionInput;
use sea_orm::sea_query::OnConflict;
use sea_orm::{
    ActiveValue::Set, ColumnTrait, ConnectionTrait, DbBackend, EntityTrait, QueryFilter,
    QueryOrder, Statement, TransactionTrait,
};
use uuid::Uuid;

use crate::entity::message;
use crate::entity::topic::{self, Status};
use crate::error::ChatError;
use crate::events::{TopicEvent, TopicEventKind};
use crate::intent::{Action, MAX_TITLE_CHARS, Routing, TopicSummary, truncate};
use crate::session_manager::principal_of;
use crate::topic_manager::TopicManager;

enum Delivery {
    Existing(i64),
    Child(i64),
    Refused(ChatError),
}

impl TopicManager {
    pub async fn send_turn(
        self: &Arc<Self>,
        user_id: Uuid,
        turn_id: Uuid,
        content: String,
    ) -> Result<Vec<i64>, ChatError> {
        let (focus_topic_id, topics) = self.session.get_session_view(user_id).await?;
        if let Some(delivered) = self.already_delivered(turn_id, &topics).await? {
            return Ok(delivered);
        }
        let actions = match self
            .intent
            .route(&summaries(&topics), focus_topic_id, &content)
            .await
        {
            // A turn with nothing to act on is answered here and now: creating a topic to ask
            // "what did you mean?" would cost an Engine execution and a minute's wait for a reply
            // the session can give instantly. This runs before `lock_turn` below, so two
            // concurrent retries of one turn_id each publish their own clarification event — that
            // is accepted and intentional, not an oversight: the event starts nothing and has no
            // side effect to double up on, so publishing it twice costs nothing a client would
            // notice.
            Routing::Clarify => {
                let session = self.session.get_or_create_session(user_id).await?;
                self.publish(
                    TopicEvent::session_wide(session.id, TopicEventKind::ClarificationNeeded)
                        .with_payload(serde_json::json!({
                            "text": crate::intent::CLARIFICATION_TEXT,
                            "turn_id": turn_id,
                            "content": content,
                        })),
                )
                .await;
                return Ok(Vec::new());
            }
            Routing::Act(actions) => actions,
        };

        let turn_guard = self.session.db.begin().await?;
        lock_turn(&turn_guard, turn_id).await?;
        // Engine reporting busy is caught here, not propagated: there is nowhere to queue this
        // turn (see `EngineError::Busy`'s doc and item 5 of the fix wave this belongs to), so
        // `turn_guard` rolls back with nothing written — the message truly was not delivered — but
        // the caller must still see `Ok`, with something visible in the transcript in place of the
        // `internal` error this used to come back as.
        let received = match self.deliver_turn(user_id, turn_id, &content, actions).await {
            Ok(received) => received,
            Err(ChatError::EngineBusy(topic_id, _message)) => {
                self.note_engine_busy(user_id, topic_id, turn_id, &content)
                    .await?;
                return Ok(Vec::new());
            }
            Err(other) => return Err(other),
        };
        turn_guard.commit().await?;
        Ok(received)
    }

    /// Publishes a visible stand-in for a turn that could not be delivered because its topic's
    /// execution was busy — see `send_turn`'s call site.
    async fn note_engine_busy(
        &self,
        user_id: Uuid,
        topic_id: i64,
        turn_id: Uuid,
        content: &str,
    ) -> Result<(), ChatError> {
        let session = self.session.get_or_create_session(user_id).await?;
        self.publish(
            TopicEvent::new(
                session.id,
                Some(topic_id),
                TopicEventKind::EngineBusy,
                Utc::now(),
            )
            .with_payload(serde_json::json!({
                "text": crate::intent::ENGINE_BUSY_TEXT,
                "turn_id": turn_id,
                "content": content,
            })),
        )
        .await;
        Ok(())
    }

    async fn deliver_turn(
        self: &Arc<Self>,
        user_id: Uuid,
        turn_id: Uuid,
        content: &str,
        actions: Vec<Action>,
    ) -> Result<Vec<i64>, ChatError> {
        let (_focus_topic_id, topics) = self.session.get_session_view(user_id).await?;

        if let Some(delivered) = self.already_delivered(turn_id, &topics).await? {
            return Ok(delivered);
        }

        let (received, new_focus) = self
            .apply_actions(user_id, &topics, content, actions)
            .await?;
        self.record_turn(turn_id, content, &received).await?;

        if let Some(topic_id) = new_focus {
            self.set_focus(user_id, topic_id).await?;
        }
        Ok(received)
    }

    async fn record_turn(
        &self,
        turn_id: Uuid,
        content: &str,
        received: &[i64],
    ) -> Result<(), ChatError> {
        let now = Utc::now();
        for topic_id in received {
            message::Entity::insert(message::ActiveModel {
                topic_id: Set(*topic_id),
                turn_id: Set(turn_id),
                content: Set(content.to_owned()),
                created_at: Set(now),
                ..Default::default()
            })
            .on_conflict(
                OnConflict::columns([message::Column::TopicId, message::Column::TurnId])
                    .do_nothing()
                    .to_owned(),
            )
            .exec_without_returning(&self.session.db)
            .await?;
        }
        Ok(())
    }

    async fn already_delivered(
        &self,
        turn_id: Uuid,
        topics: &[topic::Model],
    ) -> Result<Option<Vec<i64>>, ChatError> {
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

    async fn apply_actions(
        self: &Arc<Self>,
        user_id: Uuid,
        topics: &[topic::Model],
        content: &str,
        actions: Vec<Action>,
    ) -> Result<(Vec<i64>, Option<i64>), ChatError> {
        let mut received: Vec<i64> = Vec::new();
        let mut new_focus: Option<i64> = None;
        let mut refusal: Option<ChatError> = None;

        for action in actions {
            match action {
                Action::New { title, question } => {
                    let input_json = ExecutionInput::new(question).to_json();
                    let (topic_id, _status) =
                        self.create_topic(user_id, None, title, input_json).await?;
                    received.push(topic_id);
                    new_focus.get_or_insert(topic_id);
                }
                Action::Continue { topic_id } => {
                    let Some(topic) = topics.iter().find(|t| t.id == topic_id) else {
                        continue;
                    };
                    match self.apply_continue(user_id, topic, content).await? {
                        Delivery::Existing(id) => received.push(id),
                        Delivery::Child(id) => {
                            received.push(id);
                            new_focus.get_or_insert(id);
                        }
                        Delivery::Refused(error) => {
                            refusal.get_or_insert(error);
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

    async fn apply_continue(
        self: &Arc<Self>,
        user_id: Uuid,
        topic: &topic::Model,
        content: &str,
    ) -> Result<Delivery, ChatError> {
        match topic.status {
            Status::Completed => Ok(Delivery::Child(
                self.continue_in_child(user_id, topic, content).await?,
            )),
            Status::Running => {
                let execution_id = topic.execution_id.ok_or(ChatError::InvalidRequest(format!(
                    "topic {} has no running execution to interrupt",
                    topic.id
                )))?;
                let session = self.session.session_by_id(topic.session_id).await?;
                let input_json = ExecutionInput::new(content).to_json().to_string();
                self.engine
                    .interrupt(&principal_of(&session), execution_id, &input_json)
                    .await
                    .map_err(|error| engine_call_error(topic.id, error))?;
                Ok(Delivery::Existing(topic.id))
            }
            Status::Queued => Ok(Delivery::Existing(topic.id)),
            Status::Failed | Status::Cancelled => {
                Ok(Delivery::Refused(ChatError::TopicNotRunning(topic.id)))
            }
        }
    }

    async fn continue_in_child(
        self: &Arc<Self>,
        user_id: Uuid,
        parent: &topic::Model,
        content: &str,
    ) -> Result<i64, ChatError> {
        let question = match parent.result_summary.as_deref() {
            Some(summary) => format!("Earlier answer:\n{summary}\n\nFollow-up: {content}"),
            None => {
                tracing::warn!(
                    parent_id = parent.id,
                    "completed parent topic has no result_summary; \
                     the continuation starts without the earlier answer"
                );
                content.to_owned()
            }
        };
        let title = truncate(content, MAX_TITLE_CHARS);
        let input_json = ExecutionInput::new(question).to_json();
        let (child_id, _status) = self
            .create_topic(user_id, Some(parent.id), title, input_json)
            .await?;
        Ok(child_id)
    }
}

// `unavailable` is how Engine reports an execution that is merely mid-tick — see
// `services/engine/src/error.rs`'s `EngineError::Busy` and its `From` impl. Anything else Engine's
// `Interrupt` can fail with is a real failure, unchanged from before this distinction existed.
fn engine_call_error(topic_id: i64, error: connectrpc::ConnectError) -> ChatError {
    if error.code == connectrpc::ErrorCode::Unavailable {
        ChatError::EngineBusy(topic_id, error.to_string())
    } else {
        ChatError::Engine(error.to_string())
    }
}

async fn lock_turn<C: ConnectionTrait>(db: &C, turn_id: Uuid) -> Result<(), ChatError> {
    db.execute_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "SELECT pg_advisory_xact_lock($1)",
        [advisory_key(turn_id).into()],
    ))
    .await?;
    Ok(())
}

fn advisory_key(turn_id: Uuid) -> i64 {
    let (high, _low) = turn_id.as_u64_pair();
    i64::from_be_bytes(high.to_be_bytes())
}

fn summaries(topics: &[topic::Model]) -> Vec<TopicSummary> {
    topics
        .iter()
        .map(|topic| TopicSummary {
            id: topic.id,
            title: topic.title.clone(),
            status: topic.status,
            result_summary: topic.result_summary.clone(),
        })
        .collect()
}

#[cfg(all(test, feature = "test-support"))]
mod tests {
    use super::*;
    use crate::fakes::*;
    use std::sync::atomic::Ordering;

    #[tokio::test(flavor = "multi_thread")]
    async fn a_turn_with_nothing_to_act_on_starts_no_topic_and_says_so() {
        let (url, _) = crate::fakes::serve_decider(vec![("actionable", 0.1)]).await;
        let harness = harness_with_router(&url).await;
        let mut events = harness.manager.subscribe();

        let started = harness
            .manager
            .send_turn(OWNER, Uuid::new_v4(), "hey".to_owned())
            .await
            .expect("a clarification is a normal outcome, not an error");

        assert!(started.is_empty(), "no topic may be created: {started:?}");
        let event = events.recv().await.expect("an event");
        assert_eq!(event.kind, TopicEventKind::ClarificationNeeded);
        assert_eq!(
            event.topic_id, None,
            "a clarification belongs to the session, not a topic"
        );
        assert_eq!(
            event
                .payload
                .get("text")
                .and_then(serde_json::Value::as_str),
            Some(crate::intent::CLARIFICATION_TEXT)
        );
        assert_eq!(
            event
                .payload
                .get("content")
                .and_then(serde_json::Value::as_str),
            Some("hey"),
            "the user's own message must ride along, or a reload has nothing to show above the \
             assistant's clarification"
        );
        let (_focus, topics) = harness
            .manager
            .session
            .get_session_view(OWNER)
            .await
            .expect("view");
        assert!(topics.is_empty());

        // The guarantee has to survive a reconnect, not just live on the bus: a client that was
        // never subscribed rebuilds the transcript from what `chat.events` actually stored.
        let session = harness
            .manager
            .session_of_user(OWNER)
            .await
            .expect("session");
        let stored = harness
            .manager
            .stored_events(&session)
            .await
            .expect("replay")
            .into_iter()
            .find(|stored| stored.kind == TopicEventKind::ClarificationNeeded)
            .expect("the clarification must have been persisted, not just published live");
        assert_eq!(stored.topic_id, None);
        assert_eq!(
            TopicEventKind::from_column("clarification_needed"),
            Some(TopicEventKind::ClarificationNeeded)
        );
    }

    #[test]
    fn two_different_turns_get_two_advisory_keys() {
        let turn_id = Uuid::new_v4();
        assert_eq!(advisory_key(turn_id), advisory_key(turn_id));
        assert_ne!(advisory_key(turn_id), advisory_key(Uuid::new_v4()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn send_turn_interrupts_the_focus_topic_and_a_repeat_is_a_no_op() {
        let (_test, manager, fake) = manager_with().await;
        let user_id = Uuid::new_v4();
        let (topic_id, _) = manager
            .create_topic(user_id, None, "First".into(), serde_json::json!({}))
            .await
            .expect("create");
        let execution_id = execution_id_of(&manager, topic_id).await;

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

        manager
            .send_turn(user_id, turn_id, "hello".into())
            .await
            .expect("send_turn again");
        assert_eq!(
            fake.interrupt_count(),
            1,
            "Interrupt must not be called a second time for a repeated turn_id"
        );
        assert_eq!(stored_turns(&manager, topic_id, turn_id).await, 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn two_concurrent_retries_of_one_turn_are_delivered_once() {
        let (_test, manager, fake, router) = manager_with_router().await;
        router.answer_with(
            r#"{"actions":[{"kind":"new","title":"Claude Code","question":"What is Claude Code?"}]}"#,
        );

        for trial in 0..10 {
            let user_id = Uuid::new_v4();
            manager.session_of_user(user_id).await.expect("session");
            let turn_id = Uuid::new_v4();
            let starts_before = fake.start_execution_count();

            let retries: Vec<_> = (0..2)
                .map(|_| {
                    let manager = Arc::clone(&manager);
                    tokio::spawn(
                        async move { manager.send_turn(user_id, turn_id, "hi".into()).await },
                    )
                })
                .collect();
            let mut delivered = Vec::new();
            for retry in retries {
                delivered.push(retry.await.expect("join").expect("the retry must not fail"));
            }

            assert_eq!(
                delivered[0], delivered[1],
                "trial {trial}: both callers must be told the same topic received the turn"
            );
            let (_focus, topics) = manager
                .session
                .get_session_view(user_id)
                .await
                .expect("view");
            assert_eq!(
                topics.len(),
                1,
                "trial {trial}: one user message, one topic: {topics:?}"
            );
            assert_eq!(
                fake.start_execution_count(),
                starts_before + 1,
                "trial {trial}: and exactly one Engine execution"
            );
            assert_eq!(stored_turns(&manager, topics[0].id, turn_id).await, 1);
        }
    }

    const DECIDE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);

    #[tokio::test(flavor = "multi_thread")]
    async fn both_retries_of_one_turn_reach_the_decision_before_either_takes_the_turn_lock() {
        let (_test, manager, fake, router) = manager_with_router().await;
        router.hold_every_call_until(Arc::new(tokio::sync::Barrier::new(2)));
        let user_id = Uuid::new_v4();
        manager.session_of_user(user_id).await.expect("session");
        let turn_id = Uuid::new_v4();

        let retries: Vec<_> = (0..2)
            .map(|_| {
                let manager = Arc::clone(&manager);
                tokio::spawn(async move { manager.send_turn(user_id, turn_id, "hi".into()).await })
            })
            .collect();
        for retry in retries {
            tokio::time::timeout(DECIDE_DEADLINE, retry)
                .await
                .expect("the decision call must not run under the turn lock")
                .expect("join")
                .expect("send_turn");
        }

        let (_focus, topics) = manager
            .session
            .get_session_view(user_id)
            .await
            .expect("view");
        assert_eq!(topics.len(), 1, "the turn is still delivered exactly once");
        assert_eq!(fake.start_execution_count(), 1);
    }

    async fn stored_turns(manager: &Arc<TopicManager>, topic_id: i64, turn_id: Uuid) -> usize {
        message::Entity::find()
            .filter(message::Column::TopicId.eq(topic_id))
            .filter(message::Column::TurnId.eq(turn_id))
            .all(manager.db())
            .await
            .expect("query messages")
            .len()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_turn_for_a_failed_focus_topic_is_refused_rather_than_interrupted() {
        let (_test, manager, fake) = manager_with().await;
        let user_id = Uuid::new_v4();
        let (topic_id, _) = manager
            .create_topic(user_id, None, "First".into(), serde_json::json!({}))
            .await
            .expect("create");
        set_status(&manager, topic_id, Status::Failed).await;

        let error = manager
            .send_turn(user_id, Uuid::new_v4(), "hello".into())
            .await
            .expect_err("a finished focus topic must not accept a turn");

        assert!(matches!(error, ChatError::TopicNotRunning(id) if id == topic_id));
        assert_eq!(
            fake.interrupt_count(),
            0,
            "no Interrupt may be attempted against a finished execution"
        );
        let error: connectrpc::ConnectError = error.into();
        assert_eq!(error.code, connectrpc::ErrorCode::FailedPrecondition);
    }

    const PARENT_ANSWER: &str = "It is a coding agent.";
    const FOLLOW_UP: &str = "Which IDEs does it integrate with?";

    async fn session_with_a_completed_focus_topic(
        manager: &Arc<TopicManager>,
        fake: &FakeEngine,
        router: &FakeLlmRouter,
    ) -> (Uuid, i64, usize) {
        let user_id = Uuid::new_v4();
        let (parent_id, _) = manager
            .create_topic(
                user_id,
                None,
                "What is Claude Code?".into(),
                serde_json::json!({}),
            )
            .await
            .expect("create");
        let execution_id = arm_completed_event(manager, fake, parent_id, PARENT_ANSWER).await;
        manager.watch_topic(parent_id, execution_id).await;
        manager
            .set_focus(user_id, parent_id)
            .await
            .expect("set_focus");
        router.answer_with(&format!(
            r#"{{"actions":[{{"kind":"continue","topic_id":{parent_id}}}]}}"#
        ));
        (user_id, parent_id, fake.start_execution_count())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_turn_for_a_completed_focus_topic_starts_a_child_that_carries_the_answer_forward() {
        let (_test, manager, fake, router) = manager_with_router().await;
        let (user_id, parent_id, starts_before) =
            session_with_a_completed_focus_topic(&manager, &fake, &router).await;

        let child_id = one_topic(
            manager
                .send_turn(user_id, Uuid::new_v4(), FOLLOW_UP.into())
                .await
                .expect("a completed topic must accept a follow-up"),
        );

        assert_ne!(child_id, parent_id, "the follow-up gets its own topic");
        let child = topic_row(&manager, child_id).await;
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
        let question = ExecutionInput::in_state(&state).question;
        assert!(
            question.contains(PARENT_ANSWER),
            "the child's question must carry the parent's answer: {question}"
        );
        assert!(
            question.contains(FOLLOW_UP),
            "the child's question must carry the new turn: {question}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_retry_is_still_a_no_op_after_focus_has_moved_off_the_child() {
        let (_test, manager, fake, router) = manager_with_router().await;
        let (user_id, _parent_id, _starts) =
            session_with_a_completed_focus_topic(&manager, &fake, &router).await;
        let (sibling_id, status) = manager
            .create_topic(user_id, None, "Background".into(), serde_json::json!({}))
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
        assert_ne!(child_id, sibling_id, "the follow-up opened its own topic");
        let starts_after_child = fake.start_execution_count();
        let interrupts_after_child = fake.interrupt_count();

        let execution_id =
            arm_completed_event(&manager, &fake, child_id, "VS Code and JetBrains.").await;
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
        assert_eq!(
            fake.interrupt_count(),
            interrupts_after_child,
            "the follow-up must not be interrupted into the unrelated background topic"
        );
        assert_eq!(
            fake.start_execution_count(),
            starts_after_child,
            "and no second continuation may be started"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_repeated_turn_id_does_not_spawn_a_second_continuation() {
        let (_test, manager, fake, router) = manager_with_router().await;
        let (user_id, _parent_id, starts_before) =
            session_with_a_completed_focus_topic(&manager, &fake, &router).await;
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
            fake.start_execution_count(),
            starts_before + 1,
            "a repeated turn_id must not spawn a second continuation"
        );
        assert_eq!(stored_turns(&manager, child_id, turn_id).await, 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_turn_whose_interrupt_failed_can_be_retried_with_the_same_turn_id() {
        let (_test, manager, fake) = manager_with().await;
        let user_id = Uuid::new_v4();
        let (topic_id, _) = manager
            .create_topic(user_id, None, "First".into(), serde_json::json!({}))
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
        assert_eq!(
            stored_turns(&manager, topic_id, turn_id).await,
            0,
            "an undelivered turn must leave no dedup marker, or the retry becomes a silent no-op"
        );

        let routed = manager
            .send_turn(user_id, turn_id, "hello".into())
            .await
            .expect("the retry with the same turn_id must actually deliver");

        assert_eq!(routed, vec![topic_id]);
        assert_eq!(
            fake.interrupt_count(),
            2,
            "the retry must reach Engine, not short-circuit on a marker from the failed attempt"
        );
        assert_eq!(stored_turns(&manager, topic_id, turn_id).await, 1);
    }

    // Reproduces the bug live traffic hit: a follow-up sent while its topic's execution is still
    // running used to come back `internal` in under 200 ms with the message nowhere — no topic
    // row, no message row, nothing in the transcript. `SendTurn` must return `Ok`, and the user
    // must see something, without this turning into a queue.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_follow_up_while_engine_is_busy_is_visible_not_lost() {
        let (_test, manager, fake) = manager_with().await;
        let user_id = Uuid::new_v4();
        let (topic_id, _) = manager
            .create_topic(user_id, None, "First".into(), serde_json::json!({}))
            .await
            .expect("create");
        fake.interrupt_busy_remaining.store(1, Ordering::Relaxed);
        let mut events = manager.subscribe();
        let turn_id = Uuid::new_v4();

        let routed = manager
            .send_turn(user_id, turn_id, "still there?".into())
            .await
            .expect("SendTurn must return Ok even while Engine is busy");

        assert!(routed.is_empty(), "nothing was delivered: {routed:?}");
        assert_eq!(
            stored_turns(&manager, topic_id, turn_id).await,
            0,
            "a busy Engine delivers nothing, so no dedup marker is left behind either"
        );
        let event = events.recv().await.expect("an event");
        assert_eq!(event.kind, TopicEventKind::EngineBusy);
        assert_eq!(event.topic_id, Some(topic_id));
        assert_eq!(
            event
                .payload
                .get("text")
                .and_then(serde_json::Value::as_str),
            Some(crate::intent::ENGINE_BUSY_TEXT),
            "the user must see something in the transcript, not silence"
        );

        let retried = manager
            .send_turn(user_id, turn_id, "still there?".into())
            .await
            .expect("the retry, once Engine is no longer busy, must actually deliver");

        assert_eq!(retried, vec![topic_id]);
        assert_eq!(
            fake.interrupt_count(),
            2,
            "the retry must reach Engine again, not short-circuit on nothing having been recorded"
        );
        assert_eq!(stored_turns(&manager, topic_id, turn_id).await, 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_first_message_in_an_empty_session_becomes_a_started_topic() {
        let (_test, manager, fake, router) = manager_with_router().await;
        let user_id = Uuid::new_v4();
        router.answer_decision(None, vec![("actionable", 0.9)]);

        let topic_id = one_topic(
            manager
                .send_turn(user_id, Uuid::new_v4(), "What is Claude Code?".into())
                .await
                .expect("send_turn"),
        );

        let topic = topic_row(&manager, topic_id).await;
        assert_eq!(topic.title, "What is Claude Code?");
        assert_eq!(topic.status, Status::Running);
        assert_eq!(focus_of(&manager, user_id).await, Some(topic_id));
        let starts = fake.start_executions.lock().expect("lock");
        assert_eq!(starts.len(), 1, "the message started exactly one execution");
        let state: serde_json::Value =
            serde_json::from_str(&starts[0].input_json).expect("input is json");
        assert_eq!(
            Some(ExecutionInput::in_state(&state).question.as_str()),
            Some("What is Claude Code?"),
            "the routing decision's self-contained question is what the worker is given"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_message_naming_two_themes_opens_two_topics_and_focuses_the_first() {
        let (_test, manager, fake, router) = manager_with_router().await;
        let user_id = Uuid::new_v4();
        router.answer_decision(None, vec![("separate_themes", 0.9)]);
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
            fake.start_execution_count(),
            2,
            "both topics run in parallel"
        );
        for topic_id in &topic_ids {
            let stored = message::Entity::find()
                .filter(message::Column::TopicId.eq(*topic_id))
                .all(manager.db())
                .await
                .expect("query");
            assert_eq!(stored.len(), 1, "topic {topic_id} holds the user's message");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_continue_on_the_running_focus_interrupts_it_and_creates_no_topic() {
        let (_test, manager, fake, router) = manager_with_router().await;
        let user_id = Uuid::new_v4();
        let (topic_id, _) = manager
            .create_topic(user_id, None, "Claude Code".into(), serde_json::json!({}))
            .await
            .expect("create");
        let starts_before = fake.start_execution_count();
        router.answer_decision(Some((&format!("topic_{topic_id}"), 0.9)), vec![]);

        let routed = manager
            .send_turn(user_id, Uuid::new_v4(), "and which IDEs?".into())
            .await
            .expect("send_turn");

        assert_eq!(routed, vec![topic_id]);
        assert_eq!(fake.interrupt_count(), 1);
        assert_eq!(
            fake.start_execution_count(),
            starts_before,
            "a continuation must not start a second execution"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_routing_decision_failure_falls_back_to_continuing_the_focused_topic() {
        let (_test, manager, fake, router) = manager_with_router().await;
        let user_id = Uuid::new_v4();
        let (topic_id, _) = manager
            .create_topic(user_id, None, "Claude Code".into(), serde_json::json!({}))
            .await
            .expect("create");
        assert!(
            router.answer.lock().expect("lock").is_none(),
            "an unarmed router fails every Complete call"
        );

        let routed = manager
            .send_turn(user_id, Uuid::new_v4(), "and which IDEs?".into())
            .await
            .expect("a routing decision outage must not fail the turn");

        assert_eq!(routed, vec![topic_id]);
        assert_eq!(fake.interrupt_count(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_session_view_returns_each_topic_s_messages_in_order() {
        let (_test, manager, _fake, router) = manager_with_router().await;
        let user_id = Uuid::new_v4();
        router.answer_decision(None, vec![("actionable", 0.9)]);
        let first_turn_id = Uuid::new_v4();
        let topic_id = one_topic(
            manager
                .send_turn(user_id, first_turn_id, "What is Claude Code?".into())
                .await
                .expect("send_turn"),
        );
        router.answer_decision(Some((&format!("topic_{topic_id}"), 0.9)), vec![]);
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
        let turn_ids: Vec<Uuid> = messages[&topic_id].iter().map(|m| m.turn_id).collect();
        assert_eq!(turn_ids, vec![first_turn_id, second_turn_id]);
    }
}
