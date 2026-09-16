// Session lookup/creation — the thin read side TopicManager (Task 5) builds on. One session
// per user_id (see this plan's Task 3 design note): get_or_create_session is idempotent.

use chrono::Utc;
use sea_orm::sea_query::OnConflict;
use sea_orm::{
    ActiveValue::Set, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder,
};
use std::collections::HashMap;
use uuid::Uuid;

use crate::entity::{message, session, topic};
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
        // Two callers can reach this point for the same user at once — the chat page opens
        // `GetSession` and `StreamEvents` in parallel, and right after a `ResetSession` there is
        // no row for either of them to find. A plain INSERT then makes the loser fail with
        // «duplicate key value violates unique constraint "sessions_user_id_key"», which the
        // user saw as a 500 on the page that had just been reset. `ON CONFLICT (user_id) DO
        // NOTHING` turns the losing insert into a no-op, and the SELECT below picks up whichever
        // row won — so both callers get the same session and neither errors.
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

    /// Every user turn stored for `topic_ids`, grouped by topic and oldest first — one query for
    /// the whole session view rather than one per topic.
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

    /// The bug the user hit live: after `ResetSession` the page reopens `GetSession` and
    /// `StreamEvents` at the same time, both find no session row, and both INSERT — the loser
    /// used to fail with a unique-constraint violation and the page showed a 500.
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
