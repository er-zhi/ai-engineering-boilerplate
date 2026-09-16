// engine.graphs: one row per (graph_id, version). user_id NULL means a system graph
// (GraphBuilder output, registered at startup — see main.rs). Surrogate i64 primary key,
// (graph_id, version) as the real natural key, matching document.rs's established pattern.

use sea_orm::entity::prelude::*;

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "graphs", schema_name = "engine")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(
        unique_key = "graph_id_and_version",
        column_type = "String(StringLen::N(128))"
    )]
    pub graph_id: String,
    #[sea_orm(unique_key = "graph_id_and_version")]
    pub version: i32,
    pub user_id: Option<Uuid>,
    #[sea_orm(column_type = "JsonBinary")]
    pub definition: Json,
    pub created_at: DateTimeUtc,
}

impl ActiveModelBehavior for ActiveModel {}
