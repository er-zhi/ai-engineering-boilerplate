// chat.events: the session's append-only event log, partitioned by month, and its maintenance.

use chrono::{DateTime, Datelike, Months, TimeDelta, TimeZone, Utc};
use sea_orm::entity::prelude::*;
use sea_orm::{DbBackend, Statement};

pub mod event {
    use sea_orm::entity::prelude::*;

    #[sea_orm::model]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "events", schema_name = "chat")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i64,
        pub event_id: Uuid,
        pub session_id: Uuid,
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
const PARTITION_PREFIX: &str = "events_y";
const MONTHS_OPEN_AHEAD: u32 = 1;

pub const TABLE_STATEMENTS: [&str; 3] = [
    "CREATE TABLE IF NOT EXISTS chat.events ( \
     id bigserial NOT NULL, \
     event_id uuid NOT NULL, \
     session_id uuid NOT NULL, \
     topic_id bigint, \
     kind varchar(32) NOT NULL, \
     payload_json jsonb NOT NULL, \
     occurred_at timestamptz NOT NULL, \
     PRIMARY KEY (occurred_at, id) \
     ) PARTITION BY RANGE (occurred_at)",
    "CREATE INDEX IF NOT EXISTS events_session_id_id_idx ON chat.events (session_id, id)",
    "CREATE UNIQUE INDEX IF NOT EXISTS events_occurred_at_event_id_idx \
     ON chat.events (occurred_at, event_id)",
];

pub async fn ensure_current_and_next(
    db: &impl ConnectionTrait,
    now: DateTime<Utc>,
) -> Result<(), DbErr> {
    ensure_partition(db, now).await?;
    ensure_partition(db, add_months(month_start(now), MONTHS_OPEN_AHEAD)).await
}

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

pub async fn drop_partitions_older_than(
    db: &impl ConnectionTrait,
    now: DateTime<Utc>,
    retention_months: u32,
) -> Result<Vec<String>, DbErr> {
    let cutoff = subtract_months(month_start(now), retention_months);
    let mut dropped = Vec::new();
    for name in partition_names(db).await? {
        let Some(start) = month_of(&name) else {
            continue;
        };
        if start < cutoff {
            db.execute_unprepared(&format!("DROP TABLE IF EXISTS chat.{name}"))
                .await?;
            dropped.push(name);
        }
    }
    Ok(dropped)
}

pub async fn maintain(
    db: &impl ConnectionTrait,
    now: DateTime<Utc>,
    retention_months: Option<u32>,
) -> Result<(), DbErr> {
    ensure_current_and_next(db, now).await?;
    if let Some(months) = retention_months {
        let dropped = drop_partitions_older_than(db, now, months).await?;
        if !dropped.is_empty() {
            tracing::info!(?dropped, "dropped chat.events partitions past retention");
        }
    }
    Ok(())
}

pub async fn is_partitioned(db: &impl ConnectionTrait) -> Result<bool, DbErr> {
    let key: Option<String> = db
        .query_one_raw(Statement::from_string(
            DbBackend::Postgres,
            format!("SELECT pg_get_partkeydef('{PARENT}'::regclass) AS key"),
        ))
        .await?
        .map(|row| row.try_get("", "key"))
        .transpose()?
        .flatten();
    Ok(key.is_some_and(|key| key.contains("occurred_at")))
}

#[must_use]
pub fn clamp_to_open_window(occurred_at: DateTime<Utc>, now: DateTime<Utc>) -> DateTime<Utc> {
    let earliest = month_start(now);
    let latest = add_months(earliest, MONTHS_OPEN_AHEAD + 1) - TimeDelta::microseconds(1);
    occurred_at.clamp(earliest, latest)
}

pub async fn setup(db: &impl ConnectionTrait) -> Result<(), DbErr> {
    for statement in TABLE_STATEMENTS {
        db.execute_unprepared(statement).await?;
    }
    ensure_current_and_next(db, Utc::now()).await
}

async fn partition_names(db: &impl ConnectionTrait) -> Result<Vec<String>, DbErr> {
    let rows = db
        .query_all_raw(Statement::from_string(
            DbBackend::Postgres,
            format!(
                "SELECT child.relname AS name FROM pg_inherits \
                 JOIN pg_class AS child ON child.oid = pg_inherits.inhrelid \
                 WHERE pg_inherits.inhparent = '{PARENT}'::regclass ORDER BY child.relname"
            ),
        ))
        .await?;
    rows.iter().map(|row| row.try_get("", "name")).collect()
}

fn partition_name(month_start: DateTime<Utc>) -> String {
    format!(
        "{PARTITION_PREFIX}{:04}m{:02}",
        month_start.year(),
        month_start.month()
    )
}

fn month_of(name: &str) -> Option<DateTime<Utc>> {
    let rest = name.strip_prefix(PARTITION_PREFIX)?;
    let (year, month) = rest.split_once('m')?;
    if year.len() != 4 || month.len() != 2 {
        return None;
    }
    Utc.with_ymd_and_hms(year.parse().ok()?, month.parse().ok()?, 1, 0, 0, 0)
        .single()
}

#[must_use]
pub fn month_start(at: DateTime<Utc>) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(at.year(), at.month(), 1, 0, 0, 0)
        .single()
        .unwrap_or(at)
}

fn add_months(at: DateTime<Utc>, months: u32) -> DateTime<Utc> {
    at.checked_add_months(Months::new(months)).unwrap_or(at)
}

fn subtract_months(at: DateTime<Utc>, months: u32) -> DateTime<Utc> {
    at.checked_sub_months(Months::new(months)).unwrap_or(at)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(year: i32, month: u32, day: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, 0, 0, 0)
            .single()
            .expect("a real instant")
    }

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
            at(2027, 1, 1),
            "the upper bound rolls into the next year, and is exclusive"
        );
    }

    #[test]
    fn partition_names_round_trip_through_month_of() {
        for (year, month) in [(2026, 1), (2026, 9), (2026, 12)] {
            let start = at(year, month, 1);
            assert_eq!(month_of(&partition_name(start)), Some(start));
        }
    }

    #[test]
    fn month_of_ignores_tables_this_module_did_not_name() {
        for name in ["events", "events_backup", "events_y2026m", "sessions"] {
            assert_eq!(
                month_of(name),
                None,
                "{name} must not look like a partition"
            );
        }
    }

    #[test]
    fn a_timestamp_outside_the_open_window_is_pulled_back_into_it() {
        let now = at(2026, 9, 16);
        assert_eq!(clamp_to_open_window(at(2026, 7, 4), now), at(2026, 9, 1));
        assert_eq!(
            clamp_to_open_window(at(2027, 5, 1), now),
            at(2026, 11, 1) - TimeDelta::microseconds(1),
            "the upper bound is the last instant next month's partition accepts"
        );
        let inside = at(2026, 10, 9);
        assert_eq!(clamp_to_open_window(inside, now), inside);
    }
}
