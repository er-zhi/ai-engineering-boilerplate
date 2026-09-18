// The engine.pending_inputs table: input that arrived for an execution while it was mid-tick
// (status Running), so `interrupt` queued it here instead of refusing it. `store::commit_step`
// drains a row's queue — applying every entry, oldest first, through the same
// `engine_core::interrupt` path an idle interrupt already uses — the next time that execution's
// state is about to be written, whether that write is the tick that was in flight or a later
// one. A row is deleted the moment it is drained, applied or not, so nothing here outlives the
// execution it was meant for.

use sea_orm::entity::prelude::*;

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "pending_inputs", schema_name = "engine")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(indexed)]
    pub execution_id: Uuid,
    #[sea_orm(column_type = "JsonBinary")]
    pub input_json: Json,
    pub created_at: DateTimeUtc,
}

impl ActiveModelBehavior for ActiveModel {}
