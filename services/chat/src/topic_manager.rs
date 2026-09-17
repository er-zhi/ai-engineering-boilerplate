// How a topic comes into existence, how a session is wiped, and how watchers are recovered.

use std::sync::Arc;

use chrono::Utc;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter,
    TransactionTrait,
};
use uuid::Uuid;

use crate::classifier::TopicClassifier;
use crate::engine_client::EngineClient;
use crate::entity::topic::{self, Status};
use crate::entity::{message, session};
use crate::error::ChatError;
use crate::event_log;
use crate::event_log::event;
use crate::events::{EventBus, TopicEvent, TopicEventKind};
use common::principal::Principal;

use crate::session_manager::{SessionManager, lock_session, principal_of};

pub(crate) const AGENT_GRAPH_ID: &str = "agent";

pub struct TopicManager {
    pub session: SessionManager,
    pub(crate) engine: EngineClient,
    pub(crate) events: EventBus,
    pub(crate) classifier: TopicClassifier,
    pub(crate) reconnect_base_delay: std::time::Duration,
    pub(crate) reconnect_max_delay: std::time::Duration,
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
            reconnect_base_delay: crate::topic_watcher::RECONNECT_BASE_DELAY,
            reconnect_max_delay: crate::topic_watcher::RECONNECT_MAX_DELAY,
        })
    }

    #[cfg(feature = "test-support")]
    #[must_use]
    pub fn with_reconnect_delays(
        mut self,
        base: std::time::Duration,
        max: std::time::Duration,
    ) -> Self {
        self.reconnect_base_delay = base;
        self.reconnect_max_delay = max;
        self
    }

    pub(crate) fn db(&self) -> &DatabaseConnection {
        &self.session.db
    }

    pub async fn session_of_user(&self, user_id: Uuid) -> Result<session::Model, ChatError> {
        self.session.get_or_create_session(user_id).await
    }

    pub async fn create_topic(
        self: &Arc<Self>,
        user_id: Uuid,
        parent_id: Option<i64>,
        title: String,
        input_json: serde_json::Value,
    ) -> Result<(i64, Status), ChatError> {
        let session = self.session.get_or_create_session(user_id).await?;
        let row = self
            .insert_topic(session.id, parent_id, title, input_json)
            .await?;

        self.publish(
            TopicEvent::new(
                session.id,
                Some(row.id),
                TopicEventKind::TopicCreated,
                row.created_at,
            )
            .with_payload(serde_json::json!({"title": row.title, "parent_id": parent_id})),
        )
        .await;
        let admission = if row.status == Status::Running {
            TopicEventKind::TopicStarted
        } else {
            TopicEventKind::TopicQueued
        };
        self.publish(TopicEvent::new(
            session.id,
            Some(row.id),
            admission,
            row.created_at,
        ))
        .await;

        if let (Status::Running, Some(execution_id)) = (row.status, row.execution_id) {
            self.spawn_watch(row.id, execution_id);
        }

        Ok((row.id, row.status))
    }

    async fn insert_topic(
        &self,
        session_id: Uuid,
        parent_id: Option<i64>,
        title: String,
        input_json: serde_json::Value,
    ) -> Result<topic::Model, ChatError> {
        let txn = self.session.db.begin().await?;
        let locked = lock_session(&txn, session_id).await?;
        let principal = principal_of(&locked);
        let status = self.admission_status(&txn, session_id).await?;
        let execution_id = if status == Status::Running {
            Some(
                self.engine
                    .start_execution(&principal, AGENT_GRAPH_ID, &input_json.to_string())
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
            input_json: Set(input_json),
            result_summary: Set(None),
            artifact_ids: Set(serde_json::json!([])),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(&txn)
        .await?;

        if locked.focus_topic_id.is_none() {
            let mut active: session::ActiveModel = locked.into();
            active.focus_topic_id = Set(Some(row.id));
            active.update(&txn).await?;
        }
        txn.commit().await?;
        Ok(row)
    }

    pub async fn reset_session(&self, user_id: Uuid) -> Result<(), ChatError> {
        let session = self.session.get_or_create_session(user_id).await?;

        let txn = self.session.db.begin().await?;
        let locked = lock_session(&txn, session.id).await?;
        let principal = principal_of(&locked);

        let topics = topic::Entity::find()
            .filter(topic::Column::SessionId.eq(locked.id))
            .all(&txn)
            .await?;

        self.interrupt_running(&principal, &topics).await;

        let topic_ids: Vec<i64> = topics.iter().map(|t| t.id).collect();
        if !topic_ids.is_empty() {
            message::Entity::delete_many()
                .filter(message::Column::TopicId.is_in(topic_ids.clone()))
                .exec(&txn)
                .await?;
            topic::Entity::delete_many()
                .filter(topic::Column::Id.is_in(topic_ids))
                .exec(&txn)
                .await?;
        }
        event::Entity::delete_many()
            .filter(event::Column::SessionId.eq(locked.id))
            .filter(event::Column::OccurredAt.gte(event_log::month_start(locked.created_at)))
            .exec(&txn)
            .await?;
        session::Entity::delete_by_id(locked.id).exec(&txn).await?;
        txn.commit().await?;

        self.events.publish(TopicEvent::new(
            session.id,
            None,
            TopicEventKind::SessionReset,
            Utc::now(),
        ));
        Ok(())
    }

    async fn interrupt_running(&self, principal: &Principal, topics: &[topic::Model]) {
        for topic in topics {
            if topic.status == Status::Running
                && let Some(execution_id) = topic.execution_id
                && let Err(error) = self.engine.interrupt(principal, execution_id, "{}").await
            {
                tracing::warn!(
                    topic_id = topic.id,
                    %error,
                    "could not interrupt a running execution before deleting its topic"
                );
            }
        }
    }

    pub async fn recover(self: &Arc<Self>) -> Result<(), ChatError> {
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

    pub(crate) fn spawn_watch(self: &Arc<Self>, topic_id: i64, execution_id: Uuid) {
        let manager = Arc::clone(self);
        tokio::spawn(async move { manager.watch_topic(topic_id, execution_id).await });
    }
}

#[cfg(all(test, feature = "test-support"))]
mod tests {
    use super::*;
    use crate::fakes::*;
    use common::execution_input::ExecutionInput;

    #[tokio::test(flavor = "multi_thread")]
    async fn the_first_topic_in_a_session_starts_running_and_becomes_focus() {
        let (_test, manager, _fake) = manager_with().await;
        let user_id = Uuid::new_v4();

        let (topic_id, status) = manager
            .create_topic(
                user_id,
                None,
                "First".into(),
                ExecutionInput::new("hi").to_json(),
            )
            .await
            .expect("create");

        assert_eq!(status, Status::Running);
        assert_eq!(focus_of(&manager, user_id).await, Some(topic_id));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_second_root_topic_does_not_steal_focus() {
        let (_test, manager, _fake) = manager_with().await;
        let user_id = Uuid::new_v4();
        let (first_id, _) = manager
            .create_topic(user_id, None, "First".into(), serde_json::json!({}))
            .await
            .expect("create");

        manager
            .create_topic(user_id, None, "Second".into(), serde_json::json!({}))
            .await
            .expect("create");

        assert_eq!(focus_of(&manager, user_id).await, Some(first_id));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_started_execution_names_the_user_it_was_started_for() {
        let (_test, manager, fake) = manager_with().await;
        let user_id = Uuid::new_v4();

        manager
            .create_topic(user_id, None, "First".into(), serde_json::json!({}))
            .await
            .expect("create");

        let principals = fake.principals.lock().expect("lock");
        let started = principals.first().expect("one start_execution call");
        assert_eq!(
            started.as_ref().map(|p| p.user_id),
            Some(user_id),
            "Engine must be told whose execution this is"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reset_session_clears_everything_and_a_fresh_topic_becomes_focus_again() {
        let (_test, manager, _fake) = manager_with().await;
        let user_id = Uuid::new_v4();
        manager
            .create_topic(user_id, None, "First".into(), serde_json::json!({}))
            .await
            .expect("create");
        manager
            .create_topic(user_id, None, "Second".into(), serde_json::json!({}))
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
            .create_topic(user_id, None, "Fresh".into(), serde_json::json!({}))
            .await
            .expect("create after reset");

        assert_eq!(status, Status::Running);
        assert_eq!(
            focus_of(&manager, user_id).await,
            Some(fresh_id),
            "the first topic of the new session becomes focus again"
        );
    }
}
