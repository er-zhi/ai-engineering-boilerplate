// tool.tools: the registry — one row per tool, system (user_id NULL) or user-owned.

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

pub const MANUAL_UNIQUE_INDEX_NAME: &str = "tools_owner_slug_idx";

const UNIQUE_SLUG_PER_OWNER: &str = "CREATE UNIQUE INDEX IF NOT EXISTS tools_owner_slug_idx ON tool.tools \
     (COALESCE(user_id, '00000000-0000-0000-0000-000000000000'::uuid), slug)";

const SLUG_LOOKUP_INDEX: &str =
    "CREATE INDEX IF NOT EXISTS tools_user_slug_idx ON tool.tools (user_id, slug)";

pub const INDEX_STATEMENTS_CREATED_AFTER_SCHEMA_SYNC: [&str; 2] =
    [UNIQUE_SLUG_PER_OWNER, SLUG_LOOKUP_INDEX];
