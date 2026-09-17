// What was sent to the System One provider and what came back, kept only long enough to take a recent decision apart.

// One row per decisions row, so the table is bounded by the months partition maintenance keeps: everything
// older than PAYLOAD_RETENTION_MONTHS leaves with its partition. Nothing reads it outside the tests today;
// it is read by hand, by created_at and decision_id, when a decision has to be taken apart.

use sea_orm::entity::prelude::*;

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "decision_payloads", schema_name = "llm_router")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub decision_id: i64,
    #[sea_orm(column_type = "JsonBinary")]
    pub sent: Json,
    #[sea_orm(column_type = "JsonBinary")]
    pub received: Json,
    pub created_at: DateTimeUtc,
}

impl ActiveModelBehavior for ActiveModel {}

pub const TABLE_STATEMENT: &str = "CREATE TABLE IF NOT EXISTS llm_router.decision_payloads ( \
     id bigserial NOT NULL, \
     decision_id bigint NOT NULL, \
     sent jsonb NOT NULL, \
     received jsonb NOT NULL, \
     created_at timestamptz NOT NULL, \
     PRIMARY KEY (created_at, id), \
     UNIQUE (created_at, decision_id) \
     ) PARTITION BY RANGE (created_at)";
