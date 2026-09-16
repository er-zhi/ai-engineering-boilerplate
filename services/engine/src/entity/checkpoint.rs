// engine.checkpoints: append-only, one row per super-step. state here is the source of truth —
// executions.current_nodes is a denormalized read-optimization, this table is not.
//
// Row-count bound (gate-database → "Growth"): hot working set, like `executions` — at most
// `max_iterations` rows per live execution, and `sweep::sweep_terminal` deletes an execution's
// whole set with its row once the retention passes. After a terminal status a checkpoint is a
// restart point nobody will ever restart from; the history lives in `execution_events`.
// Hot-path query: `store::PgCheckpointStore::latest`, once per tick, by `(execution_id, step)`.

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
