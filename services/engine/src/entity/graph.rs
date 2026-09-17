// The engine.graphs table: one row per registered (graph_id, version).

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
