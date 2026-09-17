// Focus policy: the session's pointer at the topic the user is talking to.

use chrono::Utc;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter, QueryOrder,
    TransactionTrait,
};
use uuid::Uuid;

use crate::entity::session;
use crate::entity::topic::{self, Status};
use crate::error::ChatError;
use crate::events::{TopicEvent, TopicEventKind};
use crate::session_manager::lock_session;
use crate::topic_manager::TopicManager;

const REASON_USER: &str = "user";
const REASON_COMPLETION: &str = "completion";

impl TopicManager {
    pub async fn set_focus(&self, user_id: Uuid, topic_id: i64) -> Result<(), ChatError> {
        let session = self.session.get_or_create_session(user_id).await?;
        let topic = topic::Entity::find_by_id(topic_id)
            .one(self.db())
            .await?
            .filter(|t| t.session_id == session.id)
            .ok_or(ChatError::TopicNotFound(topic_id))?;

        let previous = session.focus_topic_id;
        let mut active: session::ActiveModel = self.session.session_by_id(session.id).await?.into();
        active.focus_topic_id = Set(Some(topic_id));
        active.update(self.db()).await?;

        self.publish(
            TopicEvent::new(
                session.id,
                Some(topic.id),
                TopicEventKind::FocusChanged,
                Utc::now(),
            )
            .with_payload(
                serde_json::json!({"from": previous, "to": topic_id, "reason": REASON_USER}),
            ),
        )
        .await;
        Ok(())
    }

    pub(crate) async fn update_topic_status_and_move_focus(
        &self,
        topic_id: i64,
        status: Status,
        summary: Option<String>,
    ) -> Result<(topic::Model, Option<(Option<i64>, i64)>), ChatError> {
        let session_id = self.session.session_of_topic(topic_id).await?.id;

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

    async fn move_focus_off_completed<C: sea_orm::ConnectionTrait>(
        &self,
        txn: &C,
        session: session::Model,
        completed_id: i64,
    ) -> Result<Option<(Option<i64>, i64)>, ChatError> {
        if session.focus_topic_id != Some(completed_id) {
            return Ok(None);
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

    pub(crate) async fn publish_focus_change(
        &self,
        session_id: Uuid,
        next: Option<i64>,
        from: i64,
    ) {
        self.publish(
            TopicEvent::new(session_id, next, TopicEventKind::FocusChanged, Utc::now())
                .with_payload(
                    serde_json::json!({"from": from, "to": next, "reason": REASON_COMPLETION}),
                ),
        )
        .await;
    }
}

#[cfg(all(test, feature = "test-support"))]
mod tests {
    use super::*;
    use crate::fakes::*;
    use std::sync::Arc;

    #[tokio::test(flavor = "multi_thread")]
    async fn set_focus_moves_the_pointer_and_rejects_a_foreign_topic() {
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

        assert_eq!(focus_of(&manager, user_id).await, Some(first_id));

        manager
            .set_focus(user_id, second_id)
            .await
            .expect("set_focus");
        assert_eq!(focus_of(&manager, user_id).await, Some(second_id));

        let another_users_id = Uuid::new_v4();
        let error = manager
            .set_focus(another_users_id, first_id)
            .await
            .expect_err("foreign topic must be rejected");
        assert!(matches!(error, ChatError::TopicNotFound(id) if id == first_id));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn focus_moves_to_the_next_unfinished_topic_when_the_focused_one_completes() {
        let (_test, manager, fake) = manager_with().await;
        let user_id = Uuid::new_v4();
        let (first_id, _) = manager
            .create_topic(user_id, None, "First".into(), serde_json::json!({}))
            .await
            .expect("create");
        let (second_id, _) = manager
            .create_topic(user_id, None, "Second".into(), serde_json::json!({}))
            .await
            .expect("create");
        assert_eq!(focus_of(&manager, user_id).await, Some(first_id));

        let execution_id = arm_completed_event(&manager, &fake, first_id, "done").await;
        manager.watch_topic(first_id, execution_id).await;

        assert_eq!(
            focus_of(&manager, user_id).await,
            Some(second_id),
            "focus must follow on to the next unfinished topic"
        );

        let execution_id = arm_completed_event(&manager, &fake, second_id, "also done").await;
        manager.watch_topic(second_id, execution_id).await;

        assert_eq!(
            focus_of(&manager, user_id).await,
            None,
            "with no unfinished topic left, focus is cleared"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_focus_topic_keeps_the_focus_where_it_is() {
        let (_test, manager, fake) = manager_with().await;
        let user_id = Uuid::new_v4();
        let (first_id, _) = manager
            .create_topic(user_id, None, "First".into(), serde_json::json!({}))
            .await
            .expect("create");
        manager
            .create_topic(user_id, None, "Second".into(), serde_json::json!({}))
            .await
            .expect("create");

        let execution_id = arm_failed_event(&manager, &fake, first_id, "boom").await;
        manager.watch_topic(first_id, execution_id).await;

        assert_eq!(focus_of(&manager, user_id).await, Some(first_id));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn move_focus_off_completed_cannot_race_a_concurrent_create_topic() {
        let (_test, manager, _fake) = manager_with().await;

        for i in 0..25 {
            let user_id = Uuid::new_v4();
            let (focus_id, _status) = manager
                .create_topic(user_id, None, format!("Focus{i}"), serde_json::json!({}))
                .await
                .expect("create focus topic");

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
                        .create_topic(user_id, None, format!("Sibling{i}"), serde_json::json!({}))
                        .await
                },
            );
            finish_result.expect("finish_topic");
            let (sibling_id, _sibling_status) = create_result.expect("create sibling topic");

            assert_eq!(
                focus_of(&manager, user_id).await,
                Some(sibling_id),
                "iteration {i}: focus must land on the only unfinished topic (the sibling), \
                 not be lost to the create_topic race"
            );
        }
    }
}
