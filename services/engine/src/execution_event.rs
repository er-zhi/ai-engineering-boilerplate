// The engine.execution_events table: an append-only log bounded by the monthly partitions ENGINE_EVENTS_RETENTION_MONTHS drops; StreamEvents polls it by execution_id or by user_id.

use sea_orm::entity::prelude::*;

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "execution_events", schema_name = "engine")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub event_id: Uuid,
    pub execution_id: Uuid,
    pub user_id: Option<Uuid>,
    pub version: i16,
    pub causation_id: Option<Uuid>,
    pub occurred_at: DateTimeUtc,
    #[sea_orm(column_type = "JsonBinary")]
    pub payload: Json,
}

impl ActiveModelBehavior for ActiveModel {}

pub const TABLE_STATEMENTS: [&str; 1] = ["CREATE TABLE IF NOT EXISTS engine.execution_events ( \
     id bigserial NOT NULL, \
     event_id uuid NOT NULL, \
     execution_id uuid NOT NULL, \
     user_id uuid, \
     version smallint NOT NULL, \
     causation_id uuid, \
     occurred_at timestamptz NOT NULL, \
     payload jsonb NOT NULL, \
     PRIMARY KEY (occurred_at, id) \
     ) PARTITION BY RANGE (occurred_at)"];

pub const INDEX_STATEMENTS_CREATED_AFTER_SCHEMA_SYNC: [&str; 2] = [
    "CREATE INDEX IF NOT EXISTS execution_events_execution_id_id_idx \
     ON engine.execution_events (execution_id, id)",
    "CREATE INDEX IF NOT EXISTS execution_events_user_id_id_idx \
     ON engine.execution_events (user_id, id)",
];
