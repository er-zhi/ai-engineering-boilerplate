// engine.schedules: one row per cron-scheduled graph run. A schedule is not an execution — it is
// the standing instruction to start a fresh one every time `next_run_at` comes due, which the
// scheduler loop then advances to the expression's next fire time (see scheduler.rs). Surrogate
// i64 primary key, like graph.rs; `last_execution_id` is a breadcrumb to the most recent
// execution this schedule started, not a foreign key the loop depends on.

use sea_orm::entity::prelude::*;

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "schedules", schema_name = "engine")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(column_type = "String(StringLen::N(128))")]
    pub graph_id: String,
    pub graph_version: Option<i32>, // NULL = whichever version is latest at fire time
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
