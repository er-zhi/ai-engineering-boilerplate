// What was sent to the provider and what came back, kept only long enough to take a recent call apart.

use sea_orm::entity::prelude::*;

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "request_payloads", schema_name = "llm_router")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(unique)]
    pub request_id: i64,
    #[sea_orm(column_type = "JsonBinary")]
    pub sent: Json,
    #[sea_orm(column_type = "JsonBinary")]
    pub received: Json,
    pub created_at: DateTimeUtc,
    #[sea_orm(
        default_expr = "sea_orm::sea_query::Expr::current_timestamp()",
        indexed
    )]
    pub expires_at: DateTimeUtc,
}

impl ActiveModelBehavior for ActiveModel {}
