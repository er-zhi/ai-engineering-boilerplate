// Event persistence and replay: every lifecycle event goes to chat.events and to the live bus.

use chrono::Utc;
use sea_orm::sea_query::OnConflict;
use sea_orm::{ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter, QueryOrder};
use tokio::sync::broadcast;

use crate::entity::session;
use crate::error::ChatError;
use crate::event_log::{self, event};
use crate::events::{TopicEvent, TopicEventKind};
use crate::topic_manager::TopicManager;

impl TopicManager {
    pub(crate) async fn publish(&self, event: TopicEvent) {
        self.publish_all(vec![event]).await;
    }

    pub(crate) async fn publish_all(&self, events: Vec<TopicEvent>) {
        if events
            .iter()
            .any(|event| event.kind.has_no_record_but_the_event_log())
        {
            self.store_events_logging_failure(&events).await;
            self.tell_live_subscribers(&events);
        } else {
            self.tell_live_subscribers(&events);
            self.store_events_logging_failure(&events).await;
        }
    }

    fn tell_live_subscribers(&self, events: &[TopicEvent]) {
        for event in events {
            self.events.publish(event.clone());
        }
    }

    async fn store_events_logging_failure(&self, events: &[TopicEvent]) {
        if let Err(error) = self.store_events(events).await {
            tracing::error!(%error, count = events.len(), "failed to persist chat events");
        }
    }

    async fn store_events(&self, events: &[TopicEvent]) -> Result<(), ChatError> {
        let rows: Vec<event::ActiveModel> = events.iter().map(Self::row_for).collect();
        if rows.is_empty() {
            return Ok(());
        }
        event::Entity::insert_many(rows)
            .on_conflict(
                OnConflict::columns([event::Column::OccurredAt, event::Column::EventId])
                    .do_nothing()
                    .to_owned(),
            )
            .exec_without_returning(self.db())
            .await?;
        Ok(())
    }

    fn row_for(event: &TopicEvent) -> event::ActiveModel {
        event::ActiveModel {
            event_id: Set(event.event_id),
            session_id: Set(event.session_id),
            topic_id: Set(event.topic_id),
            kind: Set(event.kind.as_column()),
            payload_json: Set(event.payload.clone()),
            occurred_at: Set(event_log::clamp_to_open_window(
                event.occurred_at,
                Utc::now(),
            )),
            ..Default::default()
        }
    }

    pub(crate) async fn stored_events(
        &self,
        session: &session::Model,
    ) -> Result<Vec<TopicEvent>, ChatError> {
        let rows = event::Entity::find()
            .filter(event::Column::SessionId.eq(session.id))
            .filter(event::Column::OccurredAt.gte(event_log::month_start(session.created_at)))
            .order_by_asc(event::Column::Id)
            .all(self.db())
            .await?;
        Ok(rows.into_iter().filter_map(stored_row_to_event).collect())
    }

    pub async fn subscribe_and_replay(
        &self,
        session: &session::Model,
    ) -> Result<(broadcast::Receiver<TopicEvent>, Vec<TopicEvent>), ChatError> {
        let live = self.events.subscribe();
        Ok((live, self.stored_events(session).await?))
    }

    /// The live bus alone, with no replay — for tests that assert on one published event rather
    /// than a session's whole history.
    #[cfg(feature = "test-support")]
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<TopicEvent> {
        self.events.subscribe()
    }
}

fn stored_row_to_event(row: event::Model) -> Option<TopicEvent> {
    let Some(kind) = TopicEventKind::from_column(&row.kind) else {
        tracing::warn!(kind = %row.kind, "skipping a stored chat event of an unknown kind");
        return None;
    };
    Some(TopicEvent {
        event_id: row.event_id,
        session_id: row.session_id,
        topic_id: row.topic_id,
        kind,
        payload: row.payload_json,
        occurred_at: row.occurred_at,
    })
}

#[cfg(all(test, feature = "test-support"))]
mod tests {
    use super::*;
    use crate::fakes::*;
    use uuid::Uuid;

    #[tokio::test(flavor = "multi_thread")]
    async fn events_are_persisted_and_replayed_in_order_then_cleared_by_a_reset() {
        let (_test, manager, _fake) = manager_with().await;
        let user_id = Uuid::new_v4();
        let (first_id, _) = manager
            .create_topic(user_id, None, "First".into(), serde_json::json!({}))
            .await
            .expect("create");
        let (second_id, _) = manager
            .create_topic(user_id, None, "Second".into(), serde_json::json!({}))
            .await
            .expect("create");
        manager
            .set_focus(user_id, second_id)
            .await
            .expect("set_focus");
        let session = manager.session_of_user(user_id).await.expect("session");

        let replay = manager.stored_events(&session).await.expect("replay");

        let kinds: Vec<String> = replay.iter().map(|e| e.kind.as_column()).collect();
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
        assert_eq!(replay[0].topic_id, Some(first_id));
        assert_eq!(replay[4].topic_id, Some(second_id));
        assert!(
            replay.iter().all(|e| e.session_id == session.id),
            "every replayed event names its session, which is what the live tail filters on"
        );

        manager.reset_session(user_id).await.expect("reset");

        assert!(
            manager
                .stored_events(&session)
                .await
                .expect("replay")
                .is_empty(),
            "a reset must leave no events behind to resurrect deleted topics"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_second_watcher_over_the_same_execution_does_not_duplicate_the_log() {
        let (_test, manager, fake) = manager_with().await;
        let user_id = Uuid::new_v4();
        let (topic_id, execution_id) = insert_unwatched_running_topic(&manager, user_id).await;
        let session = manager.session_of_user(user_id).await.expect("session");
        let replayed_by_engine = vec![
            node_completed_event(execution_id, 1),
            node_completed_event(execution_id, 2),
            completed_event_for(execution_id, 3, "done"),
        ];
        fake.arm_scripted_stream(
            execution_id,
            vec![replayed_by_engine.clone(), replayed_by_engine],
        );

        manager.watch_topic(topic_id, execution_id).await;
        manager.watch_topic(topic_id, execution_id).await;

        let replay = manager.stored_events(&session).await.expect("replay");
        let progress = replay
            .iter()
            .filter(|e| e.kind == TopicEventKind::TopicProgress)
            .count();
        assert_eq!(
            progress, 2,
            "the second watcher must not append the same Engine events again: {replay:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_notice_with_no_other_record_is_already_in_the_log_when_a_live_subscriber_hears_it() {
        for kind in [
            TopicEventKind::ClarificationNeeded,
            TopicEventKind::EngineBusy,
        ] {
            let (_test, manager, _fake) = manager_with().await;
            let session = manager
                .session_of_user(Uuid::new_v4())
                .await
                .expect("session");
            let mut live = manager.subscribe();

            manager
                .publish(TopicEvent::session_wide(session.id, kind))
                .await;

            let heard = live
                .recv()
                .await
                .expect("the live subscriber hears the notice");
            assert_eq!(heard.kind, kind);
            let replay = manager.stored_events(&session).await.expect("replay");
            assert!(
                replay
                    .iter()
                    .any(|stored| stored.event_id == heard.event_id),
                "{kind:?} must be written before it is sent live, or a client that replays in \
                 between never sees it: {replay:?}"
            );
        }
    }
}
