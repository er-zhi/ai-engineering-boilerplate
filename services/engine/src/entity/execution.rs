// engine.executions: one row per running/finished graph execution. current_nodes is a
// denormalized copy of the latest checkpoint's position — written in the same transaction as
// that checkpoint — so GetExecution never needs to join checkpoints (see the spec's "Схема БД").
// state is intentionally NOT a column here: it lives only in checkpoints.
//
// Row-count bound (gate-database → "Growth"): a hot working set, not history — it holds only
// executions that are still live, plus terminal ones inside `ENGINE_TERMINAL_RETENTION`, after
// which `sweep::sweep_terminal` deletes them. Hot-path query: `lease::claim_one_ready`, once per
// tick, covered by the partial index below.

use sea_orm::entity::prelude::*;

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "executions", schema_name = "engine")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    #[sea_orm(column_type = "String(StringLen::N(128))")]
    pub graph_id: String,
    pub graph_version: i32,
    pub user_id: Option<Uuid>,
    #[sea_orm(column_type = "String(StringLen::N(32))")]
    pub status: String, // Ready|Running|Waiting|Completed|Failed|Cancelled — Waiting's payload lives in current_nodes' sibling wait_kind column below
    #[sea_orm(column_type = "JsonBinary", nullable)]
    pub wait_kind: Option<Json>,
    #[sea_orm(column_type = "JsonBinary")]
    pub current_nodes: Json,
    pub iteration: i32,
    pub max_iterations: i32,
    pub deadline: Option<DateTimeUtc>,
    #[sea_orm(column_type = "JsonBinary")]
    pub budget: Json,
    pub lease_owner: Option<String>,
    pub lease_until: Option<DateTimeUtc>,
    pub created_at: DateTimeUtc,
    pub updated_at: DateTimeUtc,
}

impl ActiveModelBehavior for ActiveModel {}

/// Partial index under `lease::claim_one_ready`: only rows a claim can pick enter it, so the
/// terminal rows the claim will never want (the bulk of the table until `sweep::sweep_terminal`
/// takes them away) cost it nothing. Same sanctioned mechanism as the `execution_events` lookup
/// index — schema-sync has no way to express a partial index, so main.rs runs this right after it.
///
/// The predicate lists `waiting` as well, which the spec's version (`ready`/`running`) predates:
/// `claim_one_ready` grew a third disjunct for elapsed `WaitKind::Timer` rows, and Postgres only
/// uses a partial index when the query's own `WHERE` implies the index predicate. Leaving
/// `waiting` out would put claimable rows outside the index and the claim would go back to a seq
/// scan — the three statuses here are exactly the ones that query can return.
pub const INDEX_STATEMENTS_CREATED_AFTER_SCHEMA_SYNC: [&str; 1] = [
    "CREATE INDEX IF NOT EXISTS executions_claimable_updated_at_idx \
     ON engine.executions (updated_at) WHERE status IN ('ready', 'running', 'waiting')",
];
