// Creates and drops engine.execution_events' monthly partitions.

use chrono::{DateTime, Datelike, Months, TimeZone, Utc};
use sea_orm::{ConnectionTrait, DbBackend, DbErr, Statement};

const PARENT: &str = "engine.execution_events";

pub async fn ensure_current_and_next(
    db: &impl ConnectionTrait,
    now: DateTime<Utc>,
) -> Result<(), DbErr> {
    ensure_partition(db, now).await?;
    ensure_partition(db, add_months(month_start(now), 1)).await
}

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
            db.execute_unprepared(&format!("DROP TABLE IF EXISTS engine.{name}"))
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
            tracing::info!(
                ?dropped,
                "dropped execution_events partitions past retention"
            );
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
