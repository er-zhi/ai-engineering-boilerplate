// The background consumer of one topic's Engine event stream, with its reconnect backoff.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use buffa::Enumeration;
use common::proto::engine::v1::ExecutionEventKind;
use sea_orm::EntityTrait;
use uuid::Uuid;

use crate::engine_client::EngineEvent;
use crate::entity::topic::{self, Status};
use crate::error::ChatError;
use crate::events::{TopicEvent, TopicEventKind};
use crate::intent::truncate;
use crate::session_manager::principal_of;
use crate::topic_manager::TopicManager;
use crate::topic_status::status_word;

pub(crate) const RECONNECT_BASE_DELAY: Duration = Duration::from_secs(1);
pub(crate) const RECONNECT_MAX_DELAY: Duration = Duration::from_secs(30);
const MAX_REMEMBERED_EVENT_IDS: usize = 1_024;
const MAX_RESULT_SUMMARY_CHARS: usize = 8 * 1024;
const EXECUTION_EVENT_KIND_NAME_PREFIX: &str = "EXECUTION_EVENT_KIND_";

struct RecentEventIds {
    ids: HashSet<String>,
    order: VecDeque<String>,
}

impl RecentEventIds {
    fn new() -> Self {
        Self {
            ids: HashSet::with_capacity(MAX_REMEMBERED_EVENT_IDS),
            order: VecDeque::with_capacity(MAX_REMEMBERED_EVENT_IDS),
        }
    }

    fn remember(&mut self, id: &str) -> bool {
        if !self.ids.insert(id.to_owned()) {
            return false;
        }
        self.order.push_back(id.to_owned());
        if self.order.len() > MAX_REMEMBERED_EVENT_IDS
            && let Some(oldest) = self.order.pop_front()
        {
            self.ids.remove(&oldest);
        }
        true
    }
}

fn event_id_of(engine_event: &EngineEvent) -> Uuid {
    Uuid::new_v5(&Uuid::NAMESPACE_OID, engine_event.id.as_bytes())
}

impl TopicManager {
    pub async fn watch_topic(self: &Arc<Self>, topic_id: i64, execution_id: Uuid) {
        let Ok(session) = self.session.session_of_topic(topic_id).await else {
            tracing::error!(topic_id, "cannot watch a topic whose session is gone");
            return;
        };
        let principal = principal_of(&session);
        let mut seen = RecentEventIds::new();
        let mut attempt: u32 = 0;
        loop {
            let saw_terminal = self
                .consume_one_stream(topic_id, execution_id, &principal, &mut seen)
                .await;
            if saw_terminal {
                return;
            }
            if !self.wait_before_reconnect(topic_id, &mut attempt).await {
                return;
            }
        }
    }

    async fn consume_one_stream(
        self: &Arc<Self>,
        topic_id: i64,
        execution_id: Uuid,
        principal: &common::principal::Principal,
        seen: &mut RecentEventIds,
    ) -> bool {
        let mut events = match self.engine.stream_events(principal, execution_id).await {
            Ok(events) => events,
            Err(error) => {
                tracing::error!(topic_id, %error, "failed to open Engine event stream");
                return false;
            }
        };

        let mut saw_terminal = false;
        while let Some(event) = events.recv().await {
            if !seen.remember(&event.id) {
                continue;
            }
            if terminal_status(&event).is_some() {
                saw_terminal = true;
            }
            self.handle_engine_event_logged(topic_id, &event).await;
        }
        saw_terminal
    }

    async fn wait_before_reconnect(&self, topic_id: i64, attempt: &mut u32) -> bool {
        match topic::Entity::find_by_id(topic_id).one(self.db()).await {
            Ok(Some(row)) if row.status == Status::Running => {}
            Ok(_) => return false,
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

    async fn handle_engine_event_logged(self: &Arc<Self>, topic_id: i64, event: &EngineEvent) {
        if let Err(error) = self.handle_engine_event(topic_id, event).await {
            tracing::error!(topic_id, %error, "failed to handle an Engine event");
        }
    }

    async fn handle_engine_event(
        self: &Arc<Self>,
        topic_id: i64,
        event: &EngineEvent,
    ) -> Result<(), ChatError> {
        if let Some(status) = terminal_status(event) {
            return self.finish_topic(topic_id, status, event).await;
        }
        let session_id = self.session.session_of_topic(topic_id).await?.id;
        let detail: serde_json::Value = serde_json::from_str(&event.payload_json)
            .unwrap_or_else(|_| serde_json::json!({"text": event.payload_json}));
        let payload = serde_json::json!({ "step": progress_step(event.kind), "detail": detail });
        self.publish(
            TopicEvent::new(
                session_id,
                Some(topic_id),
                TopicEventKind::TopicProgress,
                event.occurred_at,
            )
            .with_event_id(event_id_of(event))
            .with_payload(payload),
        )
        .await;
        Ok(())
    }

    pub(crate) async fn finish_topic(
        self: &Arc<Self>,
        topic_id: i64,
        status: Status,
        event: &EngineEvent,
    ) -> Result<(), ChatError> {
        let summary = final_summary(event);
        let (row, focus_change) = self
            .update_topic_status_and_move_focus(topic_id, status, summary.clone())
            .await?;

        self.publish_terminal_events(&row, status, summary.as_deref())
            .await;
        if let Some((next, from)) = focus_change {
            self.publish_focus_change(row.session_id, next, from).await;
        }

        self.promote_next_queued(row.session_id).await
    }

    async fn publish_terminal_events(
        &self,
        row: &topic::Model,
        status: Status,
        summary: Option<&str>,
    ) {
        let kind = match status {
            Status::Completed => TopicEventKind::TopicCompleted,
            Status::Cancelled => TopicEventKind::TopicCancelled,
            Status::Failed | Status::Queued | Status::Running => TopicEventKind::TopicFailed,
        };
        let outcome = status_word(status);
        let one_moment_for_the_person_is_one_write = vec![
            TopicEvent::new(row.session_id, Some(row.id), kind, row.updated_at)
                .with_payload(serde_json::json!({ "summary": summary })),
            TopicEvent::new(
                row.session_id,
                Some(row.id),
                TopicEventKind::Notification,
                row.updated_at,
            )
            .with_payload(serde_json::json!({
                "kind": outcome,
                "text": summary,
            })),
        ];
        self.publish_all(one_moment_for_the_person_is_one_write)
            .await;
    }
}

fn terminal_status(event: &EngineEvent) -> Option<Status> {
    match event.kind {
        ExecutionEventKind::ExecutionCompleted => Some(Status::Completed),
        ExecutionEventKind::ExecutionFailed => Some(Status::Failed),
        ExecutionEventKind::ExecutionCancelled => Some(Status::Cancelled),
        ExecutionEventKind::Unspecified
        | ExecutionEventKind::ExecutionStarted
        | ExecutionEventKind::NodeStarted
        | ExecutionEventKind::NodeCompleted
        | ExecutionEventKind::NodeFailed
        | ExecutionEventKind::TaskOutput
        | ExecutionEventKind::Waiting
        | ExecutionEventKind::Resumed
        | ExecutionEventKind::Interrupted => None,
    }
}

fn final_summary(event: &EngineEvent) -> Option<String> {
    event
        .result
        .as_deref()
        .or(event.error.as_deref())
        .map(|summary| truncate(summary, MAX_RESULT_SUMMARY_CHARS))
}

fn progress_step(kind: ExecutionEventKind) -> String {
    kind.proto_name()
        .trim_start_matches(EXECUTION_EVENT_KIND_NAME_PREFIX)
        .to_lowercase()
}

#[cfg(all(test, feature = "test-support"))]
mod tests {
    use super::*;
    use crate::fakes::*;
    use common::proto::engine::v1::ExecutionEvent;

    #[test]
    fn the_remembered_event_ids_are_bounded_and_evict_the_oldest_first() {
        let mut seen = RecentEventIds::new();
        for index in 0..(MAX_REMEMBERED_EVENT_IDS + 10) {
            assert!(seen.remember(&format!("e{index}")));
        }

        assert_eq!(seen.ids.len(), MAX_REMEMBERED_EVENT_IDS);
        assert_eq!(seen.order.len(), MAX_REMEMBERED_EVENT_IDS);
        assert!(
            seen.remember("e0"),
            "the oldest id must have been evicted, not kept forever"
        );
        assert!(
            !seen.remember(&format!("e{}", MAX_REMEMBERED_EVENT_IDS + 5)),
            "a recent id is still recognised"
        );
    }

    #[test]
    fn one_engine_event_id_always_maps_to_the_same_row_identity() {
        let event = completed_event();
        assert_eq!(event_id_of(&event), event_id_of(&event.clone()));
        assert_ne!(event_id_of(&event), event_id_of(&completed_event()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_execution_records_the_engine_error_as_the_summary() {
        let (_test, manager, fake) = manager_with().await;
        let user_id = Uuid::new_v4();
        let (topic_id, _) = manager
            .create_topic(user_id, None, "Rust news".into(), serde_json::json!({}))
            .await
            .expect("create");
        let execution_id =
            arm_failed_event(&manager, &fake, topic_id, "brave search returned 422").await;

        manager.watch_topic(topic_id, execution_id).await;

        let finished = topic_row(&manager, topic_id).await;
        assert_eq!(finished.status, Status::Failed);
        assert_eq!(
            finished.result_summary,
            Some("brave search returned 422".to_owned())
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_cancelled_execution_is_published_as_cancelled_and_never_as_a_failure() {
        let (_test, manager, fake) = manager_with().await;
        let user_id = Uuid::new_v4();
        let (topic_id, _) = manager
            .create_topic(user_id, None, "Rust news".into(), serde_json::json!({}))
            .await
            .expect("create");
        let session = manager.session_of_user(user_id).await.expect("session");
        let execution_id = arm_cancelled_event(&manager, &fake, topic_id).await;

        manager.watch_topic(topic_id, execution_id).await;

        assert_eq!(
            topic_row(&manager, topic_id).await.status,
            Status::Cancelled
        );
        let replay = manager.stored_events(&session).await.expect("replay");
        assert!(
            replay
                .iter()
                .any(|e| e.kind == TopicEventKind::TopicCancelled),
            "a cancelled topic gets its own event kind: {replay:?}"
        );
        assert!(
            !replay.iter().any(|e| e.kind == TopicEventKind::TopicFailed),
            "a cancelled topic must never be published as a failure: {replay:?}"
        );
        let notification = replay
            .iter()
            .find(|e| e.kind == TopicEventKind::Notification)
            .expect("a terminal topic notifies");
        assert_eq!(notification.payload["kind"], "cancelled");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_engine_answer_longer_than_the_cap_is_shortened_before_it_reaches_the_row() {
        let (_test, manager, fake) = manager_with().await;
        let user_id = Uuid::new_v4();
        let (topic_id, _) = manager
            .create_topic(user_id, None, "Rust news".into(), serde_json::json!({}))
            .await
            .expect("create");
        let longer_than_the_cap = "x".repeat(MAX_RESULT_SUMMARY_CHARS + 1);
        let execution_id =
            arm_completed_event(&manager, &fake, topic_id, &longer_than_the_cap).await;

        manager.watch_topic(topic_id, execution_id).await;

        assert_eq!(
            topic_row(&manager, topic_id)
                .await
                .result_summary
                .expect("summary")
                .chars()
                .count(),
            MAX_RESULT_SUMMARY_CHARS
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn watch_topic_reconnects_after_a_stream_that_ends_without_a_terminal_event() {
        let (_test, manager, fake) = manager_with().await;
        let user_id = Uuid::new_v4();
        let (topic_id, execution_id) = insert_unwatched_running_topic(&manager, user_id).await;

        let stream_that_ends_without_a_terminal_event = vec![];
        let reconnect = vec![completed_event_for(execution_id, 1, "done")];
        fake.arm_scripted_stream(
            execution_id,
            vec![stream_that_ends_without_a_terminal_event, reconnect],
        );

        manager.watch_topic(topic_id, execution_id).await;

        assert_eq!(
            topic_row(&manager, topic_id).await.status,
            Status::Completed,
            "the reconnect must let the topic reach its terminal status"
        );
        assert_eq!(
            fake.stream_call_count(execution_id),
            2,
            "stream_events must be called again after the first stream ended without a terminal event"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_terminal_event_sharing_its_version_with_an_earlier_event_still_finishes_the_topic() {
        let (_test, manager, fake) = manager_with().await;
        let user_id = Uuid::new_v4();
        let (topic_id, execution_id) = insert_unwatched_running_topic(&manager, user_id).await;

        fake.arm_scripted_stream(
            execution_id,
            vec![vec![
                ExecutionEvent {
                    id: "e-progress".to_owned(),
                    ..node_completed_event(execution_id, 1)
                },
                ExecutionEvent {
                    id: "e-terminal".to_owned(),
                    ..completed_event_for(execution_id, 1, "done")
                },
            ]],
        );

        manager.watch_topic(topic_id, execution_id).await;

        assert_eq!(
            topic_row(&manager, topic_id).await.status,
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

    #[tokio::test(flavor = "multi_thread")]
    async fn a_progress_event_carries_the_step_it_reports_and_that_step_s_own_fields() {
        let (_test, manager, fake) = manager_with().await;
        let user_id = Uuid::new_v4();
        let (topic_id, execution_id) = insert_unwatched_running_topic(&manager, user_id).await;
        let session = manager.session_of_user(user_id).await.expect("session");

        fake.arm_scripted_stream(
            execution_id,
            vec![vec![
                node_completed_event(execution_id, 1),
                completed_event_for(execution_id, 2, "done"),
            ]],
        );
        manager.watch_topic(topic_id, execution_id).await;

        let replay = manager.stored_events(&session).await.expect("replay");
        let progress = replay
            .iter()
            .find(|e| e.kind == TopicEventKind::TopicProgress)
            .expect("one progress event");
        assert_eq!(
            progress.payload,
            serde_json::json!({"step": "node_completed", "detail": {"node": "search"}})
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn replayed_events_are_not_double_handled_after_a_reconnect() {
        let (_test, manager, fake) = manager_with().await;
        let user_id = Uuid::new_v4();
        let (topic_id, execution_id) = insert_unwatched_running_topic(&manager, user_id).await;
        let session = manager.session_of_user(user_id).await.expect("session");

        let before_the_stream_broke = vec![
            node_completed_event(execution_id, 1),
            node_completed_event(execution_id, 2),
        ];
        let engine_replay_from_version_1 = vec![
            node_completed_event(execution_id, 1),
            node_completed_event(execution_id, 2),
            completed_event_for(execution_id, 3, "done"),
        ];
        fake.arm_scripted_stream(
            execution_id,
            vec![before_the_stream_broke, engine_replay_from_version_1],
        );

        manager.watch_topic(topic_id, execution_id).await;

        assert_eq!(
            fake.stream_call_count(execution_id),
            2,
            "the broken first stream must trigger exactly one reconnect"
        );
        assert_eq!(
            topic_row(&manager, topic_id).await.status,
            Status::Completed
        );

        let replay = manager.stored_events(&session).await.expect("replay");
        let progress_count = replay
            .iter()
            .filter(|e| e.kind == TopicEventKind::TopicProgress)
            .count();
        assert_eq!(
            progress_count, 2,
            "versions 1 and 2 replayed by the reconnect must not be published a second time"
        );
    }
}
