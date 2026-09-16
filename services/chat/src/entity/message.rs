// chat.messages: user-submitted turns only — not the LLM/tool blow-by-blow, which stays in
// Engine's own execution_events. Enough to dedup by turn_id (spec: "дедуп реплик по turn_id",
// enforced here by a plain unique constraint — no separate idempotency abstraction needed,
// both columns are NOT NULL) and reconstruct which turns went where.

use sea_orm::entity::prelude::*;

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "messages", schema_name = "chat")]
#[sea_orm(unique_key = "topic_turn", columns = ["topic_id", "turn_id"])]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub topic_id: i64,
    pub turn_id: Uuid,
    #[sea_orm(column_type = "Text")]
    pub content: String,
    pub created_at: DateTimeUtc,
}

impl ActiveModelBehavior for ActiveModel {}
