// The engine.schedules table: one row per standing cron instruction, advanced in place rather than appended to; the scheduler polls it for due rows through schedules_due_idx.

use sea_orm::entity::prelude::*;

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "schedules", schema_name = "engine")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(column_type = "String(StringLen::N(128))")]
    pub graph_id: String,
    pub graph_version: Option<i32>,
    #[sea_orm(column_type = "String(StringLen::N(128))")]
    pub cron_expr: String,
    #[sea_orm(column_type = "JsonBinary")]
    pub input_json: Json,
    pub user_id: Option<Uuid>,
    pub enabled: bool,
    pub next_run_at: DateTimeUtc,
    pub last_execution_id: Option<Uuid>,
    pub created_at: DateTimeUtc,
    pub updated_at: DateTimeUtc,
}

impl ActiveModelBehavior for ActiveModel {}

pub const INDEX_STATEMENTS_CREATED_AFTER_SCHEMA_SYNC: [&str; 1] =
    ["CREATE INDEX IF NOT EXISTS schedules_due_idx \
     ON engine.schedules (next_run_at) WHERE enabled"];
