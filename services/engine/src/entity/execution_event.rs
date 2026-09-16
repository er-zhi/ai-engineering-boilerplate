// engine.execution_events: append-only. Primary key is the surrogate i64 id, which — because
// Postgres assigns it in insertion order — is also the ordering StreamEvents relies on; event_id
// (the ExecutionEvent's own Uuid, used as causation_id elsewhere) is a separate unique column,
// not the primary key, since it isn't naturally ordered. The execution_id lookup index isn't
// declared here — schema-sync doesn't cover it (same reason document_chunk.rs creates its HNSW
// and full-text indexes by hand); INDEX_STATEMENTS_CREATED_AFTER_SCHEMA_SYNC below is run by
// main.rs right after sync, the same way knowledge-base does it.

use sea_orm::entity::prelude::*;

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "execution_events", schema_name = "engine")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(unique_key = "event_id")]
    pub event_id: Uuid,
    pub execution_id: Uuid,
    pub user_id: Option<Uuid>,
    pub version: i16,
    pub causation_id: Option<Uuid>,
    pub occurred_at: DateTimeUtc,
    #[sea_orm(column_type = "JsonBinary")]
    pub payload: Json,
}

impl ActiveModelBehavior for ActiveModel {}

pub const INDEX_STATEMENTS_CREATED_AFTER_SCHEMA_SYNC: [&str; 1] = [
    "CREATE INDEX IF NOT EXISTS execution_events_execution_id_id_idx \
     ON engine.execution_events (execution_id, id)",
];
