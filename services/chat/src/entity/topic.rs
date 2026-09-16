// chat.topics: the tree of topics per session — reference data, grows with topic count, not
// time, no partitioning needed. `status` is the durable source of truth for restart recovery
// (spec: "темы восстанавливаются из лога" — here, from this row directly: Chat keeps no
// append-only event log of its own, Engine's already-partitioned execution_events is the log
// of record for a topic's execution detail).

use sea_orm::entity::prelude::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "i16", db_type = "SmallInteger")]
pub enum Status {
    #[sea_orm(num_value = 0)]
    Queued,
    #[sea_orm(num_value = 1)]
    Running,
    #[sea_orm(num_value = 2)]
    Completed,
    #[sea_orm(num_value = 3)]
    Failed,
    #[sea_orm(num_value = 4)]
    Cancelled,
}

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "topics", schema_name = "chat")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub session_id: Uuid,
    pub parent_id: Option<i64>,
    #[sea_orm(column_type = "String(StringLen::N(128))")]
    pub title: String,
    pub status: Status,
    /// Unset only while `Queued` — a queued topic has no Engine execution yet.
    pub execution_id: Option<Uuid>,
    pub result_summary: Option<String>,
    #[sea_orm(column_type = "JsonBinary")]
    pub artifact_ids: Json,
    pub created_at: DateTimeUtc,
    pub updated_at: DateTimeUtc,
}

impl ActiveModelBehavior for ActiveModel {}
