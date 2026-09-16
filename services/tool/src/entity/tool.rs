// tool.tools: the registry — one row per tool, system (user_id NULL) or user-owned. Reference
// data, grows with tool count, not with time — no partitioning needed (gate-database "Growth").
// Uniqueness per owner is NOT expressed here via unique_key (Postgres treats NULL <> NULL, so
// two system tools could share a slug) — see the manual index in main.rs.

use sea_orm::entity::prelude::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "i16", db_type = "SmallInteger")]
pub enum Risk {
    #[sea_orm(num_value = 0)]
    ReadOnly,
    #[sea_orm(num_value = 1)]
    Write,
    #[sea_orm(num_value = 2)]
    Destructive,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "i16", db_type = "SmallInteger")]
pub enum Status {
    #[sea_orm(num_value = 0)]
    Draft,
    #[sea_orm(num_value = 1)]
    Validated,
    #[sea_orm(num_value = 2)]
    Active,
}

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "tools", schema_name = "tool")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub user_id: Option<Uuid>,
    #[sea_orm(column_type = "String(StringLen::N(64))")]
    pub slug: String,
    #[sea_orm(column_type = "String(StringLen::N(128))")]
    pub name: String,
    #[sea_orm(column_type = "Text")]
    pub description: String,
    #[sea_orm(column_type = "JsonBinary")]
    pub input_schema: Json,
    #[sea_orm(column_type = "JsonBinary")]
    pub output_schema: Json,
    pub connection_id: Option<i64>,
    pub risk: Risk,
    pub timeout_seconds: i32,
    pub status: Status,
    pub created_at: DateTimeUtc,
    pub updated_at: DateTimeUtc,
}

impl ActiveModelBehavior for ActiveModel {}

/// The uniqueness `unique_key` cannot express — see this file's header comment. Run once, right
/// after schema-sync, same sanctioned mechanism as `services/engine`'s partial index.
pub const INDEX_STATEMENTS_CREATED_AFTER_SCHEMA_SYNC: [&str; 1] = [
    "CREATE UNIQUE INDEX IF NOT EXISTS tools_owner_slug_idx ON tool.tools \
     (COALESCE(user_id, '00000000-0000-0000-0000-000000000000'::uuid), slug)",
];
