// The per-session concurrency cap, and which queued topic takes the slot a finished one frees.

use chrono::Utc;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, ConnectionTrait, EntityTrait, PaginatorTrait,
    QueryFilter, QueryOrder, TransactionTrait,
};
use std::sync::Arc;
use uuid::Uuid;

use crate::entity::topic::{self, Status};
use crate::error::ChatError;
use crate::events::{TopicEvent, TopicEventKind};
use crate::session_manager::{lock_session, principal_of};
use crate::topic_manager::{AGENT_GRAPH_ID, TopicManager};

pub const MAX_CONCURRENT_TOPICS: u64 = 3;

impl TopicManager {
    pub(crate) async fn admission_status<C: ConnectionTrait>(
        &self,
        txn: &C,
        session_id: Uuid,
    ) -> Result<Status, ChatError> {
        let running = topic::Entity::find()
            .filter(topic::Column::SessionId.eq(session_id))
            .filter(topic::Column::Status.eq(Status::Running))
            .count(txn)
            .await?;
        Ok(if running < MAX_CONCURRENT_TOPICS {
            Status::Running
        } else {
            Status::Queued
        })
    }

    pub(crate) async fn promote_next_queued(
        self: &Arc<Self>,
        session_id: Uuid,
    ) -> Result<(), ChatError> {
        let txn = self.session.db.begin().await?;
        let locked = lock_session(&txn, session_id).await?;
        let principal = principal_of(&locked);
        if self.admission_status(&txn, session_id).await? != Status::Running {
            txn.commit().await?;
            return Ok(());
        }
        let Some(next) = topic::Entity::find()
            .filter(topic::Column::SessionId.eq(session_id))
            .filter(topic::Column::Status.eq(Status::Queued))
            .order_by_asc(topic::Column::CreatedAt)
            .one(&txn)
            .await?
        else {
            txn.commit().await?;
            return Ok(());
        };
        let continues = parent_execution_of(&txn, next.parent_id).await;
        let execution_id = self
            .engine
            .start_continuing(
                &principal,
                AGENT_GRAPH_ID,
                &next.input_json.to_string(),
                continues.map(|id| id.to_string()).as_deref(),
            )
            .await
            .map_err(ChatError::Engine)?;
        let mut active: topic::ActiveModel = next.clone().into();
        active.status = Set(Status::Running);
        active.execution_id = Set(Some(execution_id));
        active.updated_at = Set(Utc::now());
        active.update(&txn).await?;
        txn.commit().await?;

        self.publish(TopicEvent::new(
            session_id,
            Some(next.id),
            TopicEventKind::TopicStarted,
            Utc::now(),
        ))
        .await;
        self.spawn_watch(next.id, execution_id);
        Ok(())
    }
}

async fn parent_execution_of(
    txn: &sea_orm::DatabaseTransaction,
    parent_id: Option<i64>,
) -> Option<Uuid> {
    let parent_id = parent_id?;
    match topic::Entity::find_by_id(parent_id).one(txn).await {
        Ok(parent) => parent.and_then(|parent| parent.execution_id),
        Err(error) => {
            tracing::warn!(parent_id, %error, "could not read the earlier turn of a queued topic");
            None
        }
    }
}

#[cfg(all(test, feature = "test-support"))]
mod tests {
    use super::*;
    use crate::fakes::*;
    use common::execution_input::ExecutionInput;

    #[tokio::test(flavor = "multi_thread")]
    async fn a_fourth_concurrent_topic_is_queued_not_started() {
        let (_test, manager, _fake) = manager_with().await;
        let user_id = Uuid::new_v4();
        for i in 0..MAX_CONCURRENT_TOPICS {
            let (_id, status) = manager
                .create_topic(user_id, None, format!("T{i}"), serde_json::json!({}))
                .await
                .expect("create");
            assert_eq!(status, Status::Running);
        }

        let (_id, status) = manager
            .create_topic(user_id, None, "Overflow".into(), serde_json::json!({}))
            .await
            .expect("create");

        assert_eq!(status, Status::Queued);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn finishing_a_running_topic_promotes_its_queued_sibling() {
        let (_test, manager, fake) = manager_with().await;
        let user_id = Uuid::new_v4();
        let mut running_ids = Vec::new();
        for i in 0..MAX_CONCURRENT_TOPICS {
            let (id, status) = manager
                .create_topic(user_id, None, format!("T{i}"), serde_json::json!({}))
                .await
                .expect("create");
            assert_eq!(status, Status::Running);
            running_ids.push(id);
        }
        let queued_input = ExecutionInput::new("overflow question").to_json();
        let (queued_id, status) = manager
            .create_topic(user_id, None, "Overflow".into(), queued_input.clone())
            .await
            .expect("create");
        assert_eq!(status, Status::Queued);

        let finishing_id = running_ids[0];
        let execution_id = arm_completed_event(&manager, &fake, finishing_id, "done").await;
        let start_executions_before = fake.start_execution_count();

        manager.watch_topic(finishing_id, execution_id).await;

        let finished = topic_row(&manager, finishing_id).await;
        assert_eq!(finished.status, Status::Completed);
        assert_eq!(finished.result_summary, Some("done".to_owned()));

        let promoted = topic_row(&manager, queued_id).await;
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
            queued_input.to_string()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_create_topic_calls_cannot_exceed_the_running_limit() {
        let (_test, manager, _fake) = manager_with().await;
        let user_id = Uuid::new_v4();
        let session_id = manager.session_of_user(user_id).await.expect("session").id;

        let attempts = 2 * MAX_CONCURRENT_TOPICS;
        let mut tasks = Vec::new();
        for i in 0..attempts {
            let manager = Arc::clone(&manager);
            tasks.push(tokio::spawn(async move {
                manager
                    .create_topic(user_id, None, format!("T{i}"), serde_json::json!({}))
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
            .count(manager.db())
            .await
            .expect("count");
        assert_eq!(
            running, MAX_CONCURRENT_TOPICS,
            "the per-session concurrency cap must hold under concurrent creation"
        );
        let total = topic::Entity::find()
            .filter(topic::Column::SessionId.eq(session_id))
            .count(manager.db())
            .await
            .expect("count");
        assert_eq!(total, attempts, "every attempt is recorded, queued or not");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_promotion_never_takes_a_session_past_the_running_limit() {
        let (_test, manager, fake) = manager_with().await;
        let user_id = Uuid::new_v4();
        for i in 0..MAX_CONCURRENT_TOPICS {
            manager
                .create_topic(user_id, None, format!("T{i}"), serde_json::json!({}))
                .await
                .expect("create");
        }
        for i in 0..2 {
            let (_, status) = manager
                .create_topic(user_id, None, format!("Q{i}"), serde_json::json!({}))
                .await
                .expect("create");
            assert_eq!(status, Status::Queued);
        }
        let session_id = manager.session_of_user(user_id).await.expect("session").id;
        let before = fake.start_execution_count();

        manager
            .promote_next_queued(session_id)
            .await
            .expect("promote");
        manager
            .promote_next_queued(session_id)
            .await
            .expect("promote");

        assert_eq!(
            fake.start_execution_count(),
            before,
            "nothing has finished, so a promotion must start nothing: promotion checks the limit \
             it exists to keep rather than trusting its caller to have freed a slot"
        );
        let running = topic::Entity::find()
            .filter(topic::Column::SessionId.eq(session_id))
            .filter(topic::Column::Status.eq(Status::Running))
            .count(manager.db())
            .await
            .expect("count");
        assert_eq!(running, MAX_CONCURRENT_TOPICS);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_promotions_cannot_start_the_same_queued_topic_twice() {
        let (_test, manager, fake) = manager_with().await;
        let user_id = Uuid::new_v4();
        let mut running_ids = Vec::new();
        for i in 0..MAX_CONCURRENT_TOPICS {
            let (id, _) = manager
                .create_topic(user_id, None, format!("T{i}"), serde_json::json!({}))
                .await
                .expect("create");
            running_ids.push(id);
        }
        let (queued_id, status) = manager
            .create_topic(user_id, None, "Overflow".into(), serde_json::json!({}))
            .await
            .expect("create");
        assert_eq!(status, Status::Queued);
        let session_id = manager.session_of_user(user_id).await.expect("session").id;
        let mut settled: topic::ActiveModel = topic_row(&manager, running_ids[0]).await.into();
        settled.status = Set(Status::Completed);
        settled
            .update(manager.db())
            .await
            .expect("one topic leaves, which is the only thing that frees a slot to promote into");
        let before = fake.start_execution_count();

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
            fake.start_execution_count(),
            before + 1,
            "the one queued topic must be started exactly once, not once per concurrent promoter"
        );
        assert_eq!(topic_row(&manager, queued_id).await.status, Status::Running);
    }
}
