// Session lookup and creation, and the session-scoped reads the topic RPCs are built on.

use chrono::Utc;
use common::principal::Principal;
use sea_orm::sea_query::OnConflict;
use sea_orm::{
    ActiveValue::Set, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder,
    QuerySelect,
};
use std::collections::HashMap;
use uuid::Uuid;

use crate::entity::{message, session, topic};
use crate::error::ChatError;

pub(crate) async fn lock_session<C: sea_orm::ConnectionTrait>(
    db: &C,
    session_id: Uuid,
) -> Result<session::Model, ChatError> {
    session::Entity::find_by_id(session_id)
        .lock_exclusive()
        .one(db)
        .await?
        .ok_or_else(|| ChatError::InvalidRequest("session vanished".to_owned()))
}

#[must_use]
pub fn principal_of(session: &session::Model) -> Principal {
    Principal {
        user_id: session.user_id,
        session_id: session.id.to_string(),
    }
}

pub struct SessionManager {
    pub(crate) db: DatabaseConnection,
}

impl SessionManager {
    #[must_use]
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    pub async fn get_or_create_session(&self, user_id: Uuid) -> Result<session::Model, ChatError> {
        if let Some(existing) = session::Entity::find()
            .filter(session::Column::UserId.eq(user_id))
            .one(&self.db)
            .await?
        {
            return Ok(existing);
        }
        session::Entity::insert(session::ActiveModel {
            id: Set(Uuid::new_v4()),
            user_id: Set(user_id),
            focus_topic_id: Set(None),
            created_at: Set(Utc::now()),
        })
        .on_conflict(
            OnConflict::column(session::Column::UserId)
                .do_nothing()
                .to_owned(),
        )
        .exec_without_returning(&self.db)
        .await?;

        session::Entity::find()
            .filter(session::Column::UserId.eq(user_id))
            .one(&self.db)
            .await?
            .ok_or_else(|| ChatError::InvalidRequest("session vanished".to_owned()))
    }

    pub(crate) async fn session_by_id(
        &self,
        session_id: Uuid,
    ) -> Result<session::Model, ChatError> {
        session::Entity::find_by_id(session_id)
            .one(&self.db)
            .await?
            .ok_or_else(|| ChatError::InvalidRequest("session vanished".to_owned()))
    }

    pub(crate) async fn session_of_topic(
        &self,
        topic_id: i64,
    ) -> Result<session::Model, ChatError> {
        let session_id = topic::Entity::find_by_id(topic_id)
            .one(&self.db)
            .await?
            .ok_or(ChatError::TopicNotFound(topic_id))?
            .session_id;
        self.session_by_id(session_id).await
    }

    pub async fn messages_by_topic(
        &self,
        topic_ids: &[i64],
    ) -> Result<HashMap<i64, Vec<message::Model>>, ChatError> {
        let mut grouped: HashMap<i64, Vec<message::Model>> = HashMap::new();
        if topic_ids.is_empty() {
            return Ok(grouped);
        }
        let rows = message::Entity::find()
            .filter(message::Column::TopicId.is_in(topic_ids.to_vec()))
            .order_by_asc(message::Column::CreatedAt)
            .order_by_asc(message::Column::Id)
            .all(&self.db)
            .await?;
        for row in rows {
            grouped.entry(row.topic_id).or_default().push(row);
        }
        Ok(grouped)
    }

    pub async fn get_session_view(
        &self,
        user_id: Uuid,
    ) -> Result<(Option<i64>, Vec<topic::Model>), ChatError> {
        let session = self.get_or_create_session(user_id).await?;
        let topics = topic::Entity::find()
            .filter(topic::Column::SessionId.eq(session.id))
            .order_by_asc(topic::Column::CreatedAt)
            .all(&self.db)
            .await?;
        Ok((session.focus_topic_id, topics))
    }
}

#[cfg(all(test, feature = "test-support"))]
mod tests {
    use super::*;

    async fn manager_with() -> (crate::test_db::TestDb, SessionManager) {
        let test = crate::test_db::start().await;
        let manager = SessionManager::new(test.db.clone());
        (test, manager)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_or_create_session_is_idempotent() {
        let (_test, manager) = manager_with().await;
        let user_id = Uuid::new_v4();

        let first = manager.get_or_create_session(user_id).await.expect("first");
        let second = manager
            .get_or_create_session(user_id)
            .await
            .expect("second");

        assert_eq!(first.id, second.id);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_get_or_create_session_calls_create_exactly_one_row() {
        let (test, _manager) = manager_with().await;
        let user_id = Uuid::new_v4();

        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..8 {
            let manager = SessionManager::new(test.db.clone());
            tasks.spawn(async move { manager.get_or_create_session(user_id).await });
        }
        let mut ids = Vec::new();
        while let Some(joined) = tasks.join_next().await {
            let session = joined
                .expect("join")
                .expect("a concurrent caller must not see a unique-violation error");
            ids.push(session.id);
        }

        assert!(
            ids.windows(2).all(|pair| pair[0] == pair[1]),
            "every caller must get the same session: {ids:?}"
        );
        let rows = session::Entity::find()
            .filter(session::Column::UserId.eq(user_id))
            .all(&test.db)
            .await
            .expect("query");
        assert_eq!(rows.len(), 1, "exactly one session row per user");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_session_view_starts_empty_with_no_focus() {
        let (_test, manager) = manager_with().await;
        let user_id = Uuid::new_v4();

        let (focus, topics) = manager.get_session_view(user_id).await.expect("view");

        assert_eq!(focus, None);
        assert!(topics.is_empty());
    }
}
