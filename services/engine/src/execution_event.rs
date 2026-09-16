// engine.execution_events: the append-only log, and the one table in this service that grows with
// time rather than with entities — so it is `PARTITION BY RANGE (occurred_at)`, monthly, from the
// first version (gate-database → "Growth": retention here is DROP PARTITION, never DELETE, and
// converting a full table to partitions later is a rewrite under lock). `partition.rs` creates and
// drops the monthly children; this module owns the parent's definition.
//
// Row-count bound: none by row — a few events per super-step, unbounded over time, bounded only in
// months by `ENGINE_EVENTS_RETENTION_MONTHS` (unset by default = kept forever; this is the durable
// history `sweep.rs` deletes the hot tables against, so it is never trimmed by omission).
// Hot-path queries: `stream.rs` alone, always `WHERE execution_id = $1 AND id > $2 ORDER BY id`
// under `execution_events_execution_id_id_idx`. The tick path only appends here; no tick query
// ever joins this table to `executions`.
//
// Why this entity deliberately does NOT live under `crate::entity::` — the module path that
// `get_schema_registry("engine::entity::*")` globs (sea-orm 2.0.2 selects registry entries by
// `module_path!()` prefix, see sea_orm::EntityRegistry::build_schema):
//
//   1. schema-sync cannot express a partitioned parent at all — it emits a plain CREATE TABLE.
//   2. Worse, sync is not inert against one created by hand. Measured against sea-orm 2.0.2:
//      with `#[sea_orm(unique_key = ...)]` on `event_id`, every sync fails outright with
//      «unique constraint on partitioned table must include all partitioning columns»; without
//      it, sync *silently drops* the table's own UNIQUE (occurred_at, event_id) — its last pass
//      removes unique keys it finds in the database but not on the entity.
//
// So the table is created by the literal statements below, before any of this service's
// partition management, and schema-sync never sees it. The four sync-managed entities stay under
// `crate::entity::` and the glob keeps covering them automatically.
//
// The entity still declares `id` as its single primary key: that is the ORM's row identity (and
// the `RETURNING id` an insert needs), not a claim about the database's constraint, which is
// PRIMARY KEY (occurred_at, id) — Postgres requires the partition key in every unique constraint
// on a partitioned table. Because `id` is one `bigserial` sequence shared by every partition it
// is still globally monotonic in insertion order, which is exactly what StreamEvents orders by.

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

/// The partitioned parent. Run before anything inserts an event; `IF NOT EXISTS` makes it a no-op
/// on every start after the first.
pub const TABLE_STATEMENTS: [&str; 1] = ["CREATE TABLE IF NOT EXISTS engine.execution_events ( \
     id bigserial NOT NULL, \
     event_id uuid NOT NULL, \
     execution_id uuid NOT NULL, \
     user_id uuid, \
     version smallint NOT NULL, \
     causation_id uuid, \
     occurred_at timestamptz NOT NULL, \
     payload jsonb NOT NULL, \
     PRIMARY KEY (occurred_at, id), \
     UNIQUE (occurred_at, event_id) \
     ) PARTITION BY RANGE (occurred_at)"];

/// `StreamEvents`' lookup index, created on the parent so every partition inherits it.
pub const INDEX_STATEMENTS_CREATED_AFTER_SCHEMA_SYNC: [&str; 1] = [
    "CREATE INDEX IF NOT EXISTS execution_events_execution_id_id_idx \
     ON engine.execution_events (execution_id, id)",
];
