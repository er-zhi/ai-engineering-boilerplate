// chat.events: the session's own append-only event log. Until now Chat published lifecycle
// events to an in-memory broadcast channel only, so a reload lost everything that had happened —
// the debug panel started empty and `StreamEvents` could offer nothing but a synthetic snapshot.
// This table is what `StreamEvents` replays before attaching the live tail.
//
// Row-count bound: none by row — a handful of events per topic plus one per Engine progress
// event, unbounded over time. So it is an append-only log in gate-database's «Growth» sense and
// is `PARTITION BY RANGE (occurred_at)`, monthly, from its first version: retention is
// `DROP TABLE` of one child partition, never a `DELETE` of millions of rows, and converting a
// populated table to a partitioned one later is a full rewrite under lock.
//
// Hot-path queries: `StreamEvents`' replay alone — `WHERE session_id = $1 ORDER BY id`, served by
// `events_session_id_id_idx` below; plus `ResetSession`'s per-session delete, which is a domain
// operation on one user's rows, not the retention path.
//
// Why this entity deliberately does NOT live under `crate::entity::` — the module path
// `get_schema_registry("chat::entity::*")` globs: schema-sync emits a plain `CREATE TABLE` and
// cannot express a partitioned parent at all, and it is not inert against one created by hand
// (see the long note in `services/engine/src/execution_event.rs`, which hit exactly this). The
// table is created by the literal statements below, and schema-sync never sees it. The three
// sync-managed entities stay under `crate::entity::` and the glob keeps covering them.
//
// The entity declares `id` as its single primary key: that is the ORM's row identity, not a claim
// about the database constraint, which is `PRIMARY KEY (occurred_at, id)` — Postgres requires the
// partition key in every unique constraint on a partitioned table. `id` is one `bigserial`
// sequence shared by all partitions, so it stays globally monotonic in insertion order, which is
// what the replay orders by.

use chrono::{DateTime, Datelike, Months, TimeZone, Utc};
use sea_orm::entity::prelude::*;

pub mod event {
    use sea_orm::entity::prelude::*;

    #[sea_orm::model]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "events", schema_name = "chat")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i64,
        pub session_id: Uuid,
        /// Unset for session-wide events (`session_reset`, a `focus_changed` that cleared focus).
        pub topic_id: Option<i64>,
        #[sea_orm(column_type = "String(StringLen::N(32))")]
        pub kind: String,
        #[sea_orm(column_type = "JsonBinary")]
        pub payload_json: Json,
        pub occurred_at: DateTimeUtc,
    }

    impl ActiveModelBehavior for ActiveModel {}
}

const PARENT: &str = "chat.events";

/// The partitioned parent plus its one lookup index. Run before anything inserts an event;
/// `IF NOT EXISTS` makes every start after the first a no-op.
pub const TABLE_STATEMENTS: [&str; 2] = [
    "CREATE TABLE IF NOT EXISTS chat.events ( \
     id bigserial NOT NULL, \
     session_id uuid NOT NULL, \
     topic_id bigint, \
     kind varchar(32) NOT NULL, \
     payload_json jsonb NOT NULL, \
     occurred_at timestamptz NOT NULL, \
     PRIMARY KEY (occurred_at, id) \
     ) PARTITION BY RANGE (occurred_at)",
    "CREATE INDEX IF NOT EXISTS events_session_id_id_idx ON chat.events (session_id, id)",
];

/// Creates the partitions for the month containing `now` and the one after it. A range with no
/// partition makes the INSERT itself fail, so next month's is always opened a month ahead; the
/// service re-runs this daily so a long-lived process never walks off the end.
pub async fn ensure_current_and_next(
    db: &impl ConnectionTrait,
    now: DateTime<Utc>,
) -> Result<(), DbErr> {
    ensure_partition(db, now).await?;
    ensure_partition(db, add_months(month_start(now), 1)).await
}

/// Creates the partition holding the month that contains `month`.
///
/// One of gate-database's sanctioned literal statements: partition DDL has no query-builder form
/// and `FOR VALUES FROM (...) TO (...)` takes no bind parameters at all. Every value interpolated
/// here comes from `chrono` arithmetic or from a name this module itself formatted — never from a
/// request.
pub async fn ensure_partition(
    db: &impl ConnectionTrait,
    month: DateTime<Utc>,
) -> Result<(), DbErr> {
    let start = month_start(month);
    let end = add_months(start, 1);
    db.execute_unprepared(&format!(
        "CREATE TABLE IF NOT EXISTS chat.{} PARTITION OF {PARENT} \
         FOR VALUES FROM ('{}') TO ('{}')",
        partition_name(start),
        start.to_rfc3339(),
        end.to_rfc3339(),
    ))
    .await?;
    Ok(())
}

/// Creates the table, its index and the current window of partitions — the whole setup, called
/// from `main.rs` at startup and from the test database bootstrap.
pub async fn setup(db: &impl ConnectionTrait) -> Result<(), DbErr> {
    for statement in TABLE_STATEMENTS {
        db.execute_unprepared(statement).await?;
    }
    ensure_current_and_next(db, Utc::now()).await
}

fn partition_name(month_start: DateTime<Utc>) -> String {
    format!(
        "events_y{:04}m{:02}",
        month_start.year(),
        month_start.month()
    )
}

fn month_start(at: DateTime<Utc>) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(at.year(), at.month(), 1, 0, 0, 0)
        .single()
        .unwrap_or(at)
}

fn add_months(at: DateTime<Utc>, months: u32) -> DateTime<Utc> {
    at.checked_add_months(Months::new(months)).unwrap_or(at)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_partition_spans_exactly_its_month() {
        let end_of_year = Utc
            .with_ymd_and_hms(2026, 12, 31, 23, 59, 59)
            .single()
            .expect("a real instant");
        let start = month_start(end_of_year);
        assert_eq!(partition_name(start), "events_y2026m12");
        assert_eq!(
            add_months(start, 1),
            Utc.with_ymd_and_hms(2027, 1, 1, 0, 0, 0)
                .single()
                .expect("a real instant"),
            "the upper bound rolls into the next year, and is exclusive"
        );
    }
}
