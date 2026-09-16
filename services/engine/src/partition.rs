// Monthly partitions of engine.execution_events (see execution_event.rs for why that table is
// partitioned and why schema-sync never touches it).
//
// Two jobs, both idempotent and both cheap enough to run from `wakeup.rs`'s fallback-interval arm
// once a day, as a sibling of `sweep::sweep_terminal`:
//   * make sure this month's and next month's partitions exist — a range with no partition makes
//     the INSERT itself fail, so next month's is always created a month ahead;
//   * when `ENGINE_EVENTS_RETENTION_MONTHS` is set, DROP the partitions that fell out of the
//     window. Retention on a log this size is a `DROP TABLE` of one child, never a `DELETE` of
//     millions of rows (gate-database → "Growth").
//
// These are the sanctioned literal statements, like the `CREATE INDEX` ones: partition DDL has no
// query-builder form, and `FOR VALUES FROM (...) TO (...)` takes no bind parameters at all. Every
// value interpolated below comes from `chrono` arithmetic on a `DateTime<Utc>` or from a name this
// module itself formatted — never from a request.

use chrono::{DateTime, Datelike, Months, TimeZone, Utc};
use sea_orm::{ConnectionTrait, DbBackend, DbErr, Statement};

const PARENT: &str = "engine.execution_events";

/// Creates the partitions for the month containing `now` and the one after it, if they aren't
/// there yet. Called at startup and once a day from the wakeup loop.
pub async fn ensure_current_and_next(
    db: &impl ConnectionTrait,
    now: DateTime<Utc>,
) -> Result<(), DbErr> {
    ensure_partition(db, now).await?;
    ensure_partition(db, add_months(month_start(now), 1)).await
}

/// Creates the partition holding the month that contains `month`.
pub async fn ensure_partition(
    db: &impl ConnectionTrait,
    month: DateTime<Utc>,
) -> Result<(), DbErr> {
    let start = month_start(month);
    let end = add_months(start, 1);
    db.execute_unprepared(&format!(
        "CREATE TABLE IF NOT EXISTS engine.{} PARTITION OF {PARENT} \
         FOR VALUES FROM ('{}') TO ('{}')",
        partition_name(start),
        start.to_rfc3339(),
        end.to_rfc3339(),
    ))
    .await?;
    Ok(())
}

/// Drops every partition whose month ends at or before `retention_months` before the month
/// containing `now`, and returns the names dropped. A partition still holding part of the window
/// is left whole — retention here is a month's granularity by construction.
pub async fn drop_partitions_older_than(
    db: &impl ConnectionTrait,
    now: DateTime<Utc>,
    retention_months: u32,
) -> Result<Vec<String>, DbErr> {
    let cutoff = subtract_months(month_start(now), retention_months);
    let mut dropped = Vec::new();
    for name in partition_names(db).await? {
        let Some(start) = month_of(&name) else {
            continue; // not one of ours — never drop a table this module did not name
        };
        if start < cutoff {
            db.execute_unprepared(&format!("DROP TABLE IF EXISTS engine.{name}"))
                .await?;
            dropped.push(name);
        }
    }
    Ok(dropped)
}

/// The daily maintenance pass: always keep the window ahead open, and close the tail only when a
/// retention is configured (unset = keep forever, which is the default and the spec's position).
pub async fn maintain(
    db: &impl ConnectionTrait,
    now: DateTime<Utc>,
    retention_months: Option<u32>,
) -> Result<(), DbErr> {
    ensure_current_and_next(db, now).await?;
    if let Some(months) = retention_months {
        let dropped = drop_partitions_older_than(db, now, months).await?;
        if !dropped.is_empty() {
            tracing::info!(
                ?dropped,
                "dropped execution_events partitions past retention"
            );
        }
    }
    Ok(())
}

/// Whether `execution_events` is the partitioned parent this module expects.
///
/// Worth asking because of the one way `TABLE_STATEMENTS`' `CREATE TABLE IF NOT EXISTS` can
/// quietly do nothing: a database that still holds the plain table from before partitioning. The
/// service keeps working in that state — it is the same columns — but silently without the
/// Growth guarantee, so startup says so out loud rather than leaving it to be discovered later.
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

/// The parent's current children, straight from the catalog — there is no ORM entity for
/// `pg_inherits`, and this reads no application rows.
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
        "execution_events_y{:04}m{:02}",
        month_start.year(),
        month_start.month()
    )
}

/// The inverse of [`partition_name`]: `None` for any table name this module did not produce.
fn month_of(name: &str) -> Option<DateTime<Utc>> {
    let rest = name.strip_prefix("execution_events_y")?;
    let (year, month) = rest.split_once('m')?;
    if year.len() != 4 || month.len() != 2 {
        return None;
    }
    Utc.with_ymd_and_hms(year.parse().ok()?, month.parse().ok()?, 1, 0, 0, 0)
        .single()
}

fn month_start(at: DateTime<Utc>) -> DateTime<Utc> {
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

    #[test]
    fn partition_names_round_trip_through_month_of() {
        for (year, month) in [(2026, 1), (2026, 9), (2026, 12)] {
            let start = Utc
                .with_ymd_and_hms(year, month, 1, 0, 0, 0)
                .single()
                .expect("a real month");
            assert_eq!(month_of(&partition_name(start)), Some(start));
        }
    }

    #[test]
    fn month_of_ignores_tables_this_module_did_not_name() {
        for name in [
            "execution_events",
            "execution_events_backup",
            "execution_events_y2026m",
            "executions",
        ] {
            assert_eq!(
                month_of(name),
                None,
                "{name} must not look like a partition"
            );
        }
    }

    #[test]
    fn a_partition_spans_exactly_its_month() {
        let start = month_start(
            Utc.with_ymd_and_hms(2026, 12, 31, 23, 59, 59)
                .single()
                .expect("a real instant"),
        );
        assert_eq!(partition_name(start), "execution_events_y2026m12");
        assert_eq!(
            add_months(start, 1),
            Utc.with_ymd_and_hms(2027, 1, 1, 0, 0, 0)
                .single()
                .expect("a real instant"),
            "the upper bound rolls into the next year, and is exclusive"
        );
    }
}
