// What was sent to the provider and what came back, kept only long enough to take a recent call apart.

// One row per requests row, so the table is bounded by the months partition maintenance keeps: everything
// older than PAYLOAD_RETENTION_MONTHS leaves with its partition. Nothing reads it outside the tests today;
// it is read by hand, by created_at and request_id, when a call has to be taken apart.

use sea_orm::entity::prelude::*;

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "request_payloads", schema_name = "llm_router")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub request_id: i64,
    #[sea_orm(column_type = "JsonBinary")]
    pub sent: Json,
    #[sea_orm(column_type = "JsonBinary")]
    pub received: Json,
    pub created_at: DateTimeUtc,
}

impl ActiveModelBehavior for ActiveModel {}

pub const TABLE_STATEMENT: &str = "CREATE TABLE IF NOT EXISTS llm_router.request_payloads ( \
     id bigserial NOT NULL, \
     request_id bigint NOT NULL, \
     sent jsonb NOT NULL, \
     received jsonb NOT NULL, \
     created_at timestamptz NOT NULL, \
     PRIMARY KEY (created_at, id), \
     UNIQUE (created_at, request_id) \
     ) PARTITION BY RANGE (created_at)";
