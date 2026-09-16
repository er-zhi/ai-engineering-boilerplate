// engine.checkpoints: append-only, one row per super-step. state here is the source of truth —
// executions.current_nodes is a denormalized read-optimization, this table is not.

use sea_orm::entity::prelude::*;

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "checkpoints", schema_name = "engine")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(unique_key = "execution_and_step")]
    pub execution_id: Uuid,
    #[sea_orm(unique_key = "execution_and_step")]
    pub step: i32,
    pub schema_version: i16,
    #[sea_orm(column_type = "JsonBinary")]
    pub state: Json,
    #[sea_orm(column_type = "JsonBinary")]
    pub current_nodes: Json,
    pub created_at: DateTimeUtc,
}

impl ActiveModelBehavior for ActiveModel {}
