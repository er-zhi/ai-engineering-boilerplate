// chat.topics: the tree of topics per session, and the durable status restart recovery reads.

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
    pub execution_id: Option<Uuid>,
    #[sea_orm(column_type = "JsonBinary", default_value = "{}")]
    pub input_json: Json,
    #[sea_orm(column_type = "Text")]
    pub result_summary: Option<String>,
    #[sea_orm(column_type = "JsonBinary")]
    pub artifact_ids: Json,
    pub created_at: DateTimeUtc,
    pub updated_at: DateTimeUtc,
}

impl ActiveModelBehavior for ActiveModel {}

pub const INDEX_STATEMENTS_CREATED_AFTER_SCHEMA_SYNC: [&str; 1] = [
    "CREATE INDEX IF NOT EXISTS topics_session_status_created_idx \
     ON chat.topics (session_id, status, created_at)",
];
