// engine.executions: one row per running/finished graph execution. current_nodes is a
// denormalized copy of the latest checkpoint's position — written in the same transaction as
// that checkpoint — so GetExecution never needs to join checkpoints (see the spec's "Схема БД").
// state is intentionally NOT a column here: it lives only in checkpoints.

use sea_orm::entity::prelude::*;

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "executions", schema_name = "engine")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    #[sea_orm(column_type = "String(StringLen::N(128))")]
    pub graph_id: String,
    pub graph_version: i32,
    pub user_id: Option<Uuid>,
    #[sea_orm(column_type = "String(StringLen::N(32))")]
    pub status: String, // Ready|Running|Waiting|Completed|Failed|Cancelled — Waiting's payload lives in current_nodes' sibling wait_kind column below
    #[sea_orm(column_type = "JsonBinary", nullable)]
    pub wait_kind: Option<Json>,
    #[sea_orm(column_type = "JsonBinary")]
    pub current_nodes: Json,
    pub iteration: i32,
    pub max_iterations: i32,
    pub deadline: Option<DateTimeUtc>,
    #[sea_orm(column_type = "JsonBinary")]
    pub budget: Json,
    pub lease_owner: Option<String>,
    pub lease_until: Option<DateTimeUtc>,
    pub created_at: DateTimeUtc,
    pub updated_at: DateTimeUtc,
}

impl ActiveModelBehavior for ActiveModel {}
