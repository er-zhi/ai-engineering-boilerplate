// gateway.sessions: the CacheStore-backed table, holding one row per opaque session token. The key is never the raw token — only its hash, so a row leak alone does not hand over a valid session.

use sea_orm::entity::prelude::*;

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "sessions", schema_name = "gateway")]
pub struct Model {
    #[sea_orm(
        primary_key,
        auto_increment = false,
        column_type = "String(StringLen::N(512))"
    )]
    pub key: String,
    #[sea_orm(column_type = "VarBinary(StringLen::None)")]
    pub value: Vec<u8>,
    #[sea_orm(indexed)]
    pub expires_at: DateTimeUtc,
}

impl ActiveModelBehavior for ActiveModel {}
