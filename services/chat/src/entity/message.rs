// chat.messages: one row per user turn, unique per (topic, turn) so a retried turn lands once.

use sea_orm::entity::prelude::*;

pub const MAX_CONTENT_CHARS: usize = 8 * 1024;

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "messages", schema_name = "chat")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(unique_key = "topic_turn")]
    pub topic_id: i64,
    #[sea_orm(unique_key = "topic_turn")]
    pub turn_id: Uuid,
    #[sea_orm(column_type = "String(StringLen::N(MAX_CONTENT_CHARS as u32))")]
    pub content: String,
    pub created_at: DateTimeUtc,
}

impl ActiveModelBehavior for ActiveModel {}

#[cfg(all(test, feature = "test-support"))]
mod tests {
    use sea_orm::{ConnectionTrait, Statement};

    #[tokio::test(flavor = "multi_thread")]
    async fn schema_sync_creates_the_unique_index_send_turn_dedups_on() {
        let test = crate::test_db::start().await;

        let found = test
            .db
            .query_one_raw(Statement::from_string(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT indexdef FROM pg_indexes \
                 WHERE schemaname = 'chat' AND tablename = 'messages' \
                 AND indexdef ILIKE '%UNIQUE%' AND indexdef ILIKE '%topic_id%' \
                 AND indexdef ILIKE '%turn_id%'"
                    .to_owned(),
            ))
            .await
            .expect("query pg_indexes");

        assert!(
            found.is_some(),
            "chat.messages has no unique index on (topic_id, turn_id); \
             a struct-level sea_orm unique_key only generates a find_by accessor"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_content_column_is_as_wide_as_the_entry_allows_and_no_wider() {
        let test = crate::test_db::start().await;

        let width = test
            .db
            .query_one_raw(Statement::from_string(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT character_maximum_length FROM information_schema.columns \
                 WHERE table_schema = 'chat' AND table_name = 'messages' \
                 AND column_name = 'content'"
                    .to_owned(),
            ))
            .await
            .expect("query information_schema")
            .expect("chat.messages.content exists")
            .try_get_by_index::<Option<i32>>(0)
            .expect("read the column width");

        assert_eq!(
            width,
            Some(i32::try_from(super::MAX_CONTENT_CHARS).expect("the cap fits an i32")),
            "an unbounded content column leaves the RPC entry check as the only bound"
        );
    }
}
