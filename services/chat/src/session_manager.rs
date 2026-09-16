// Session lookup/creation — the thin read side TopicManager (Task 5) builds on. One session
// per user_id (see this plan's Task 3 design note): get_or_create_session is idempotent.

use chrono::Utc;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter,
    QueryOrder,
};
use uuid::Uuid;

use crate::entity::{session, topic};
use crate::error::ChatError;

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
        let row = session::ActiveModel {
            id: Set(Uuid::new_v4()),
            user_id: Set(user_id),
            focus_topic_id: Set(None),
            created_at: Set(Utc::now()),
        }
        .insert(&self.db)
        .await?;
        Ok(row)
    }

    /// `(focus_topic_id, every topic in the session)` — backs the `GetSession` RPC.
    pub async fn get_session_view(
        &self,
        user_id: Uuid,
    ) -> Result<(Option<i64>, Vec<topic::Model>), ChatError> {
        let session = self.get_or_create_session(user_id).await?;
        // Oldest first, explicitly. Without an ORDER BY this is heap order, which Postgres
        // reshuffles as rows are UPDATEd — and topic rows are updated on every status change. The
        // client relies on this order both to draw the topic panel and to pick the newest topic
        // when the server has cleared focus, so an arbitrary order there attaches a follow-up to
        // whichever conversation happened to sort last.
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
    async fn get_session_view_starts_empty_with_no_focus() {
        let (_test, manager) = manager_with().await;
        let user_id = Uuid::new_v4();

        let (focus, topics) = manager.get_session_view(user_id).await.expect("view");

        assert_eq!(focus, None);
        assert!(topics.is_empty());
    }
}
