// Monthly range partitions: what a month's partition is named, the bounds it spans, which months must be open, and the DDL that opens and drops them.

use chrono::{DateTime, Datelike, Months, TimeDelta, TimeZone, Utc};
use sea_orm::{ConnectionTrait, DbBackend, DbErr, Statement};

#[must_use]
pub fn month_start(at: DateTime<Utc>) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(at.year(), at.month(), 1, 0, 0, 0)
        .single()
        .unwrap_or(at)
}

#[must_use]
pub fn add_months(at: DateTime<Utc>, months: u32) -> DateTime<Utc> {
    at.checked_add_months(Months::new(months)).unwrap_or(at)
}

#[must_use]
pub fn subtract_months(at: DateTime<Utc>, months: u32) -> DateTime<Utc> {
    at.checked_sub_months(Months::new(months)).unwrap_or(at)
}

#[must_use]
pub fn clamp_to_open_window(
    at: DateTime<Utc>,
    now: DateTime<Utc>,
    months_open_ahead: u32,
) -> DateTime<Utc> {
    let earliest = month_start(now);
    let last_instant_the_open_months_accept =
        add_months(earliest, months_open_ahead + 1) - TimeDelta::microseconds(1);
    at.clamp(earliest, last_instant_the_open_months_accept)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Monthly {
    schema: &'static str,
    parent: &'static str,
    key_column: &'static str,
}

impl Monthly {
    #[must_use]
    pub const fn new(schema: &'static str, parent: &'static str, key_column: &'static str) -> Self {
        Self {
            schema,
            parent,
            key_column,
        }
    }

    #[must_use]
    pub const fn parent(self) -> &'static str {
        self.parent
    }

    #[must_use]
    pub fn partition_name(self, month: DateTime<Utc>) -> String {
        let start = month_start(month);
        format!("{}_y{:04}m{:02}", self.parent, start.year(), start.month())
    }

    #[must_use]
    pub fn month_of(self, name: &str) -> Option<DateTime<Utc>> {
        let rest = name.strip_prefix(self.parent)?.strip_prefix("_y")?;
        let (year, month) = rest.split_once('m')?;
        if year.len() != 4 || month.len() != 2 {
            return None;
        }
        Utc.with_ymd_and_hms(year.parse().ok()?, month.parse().ok()?, 1, 0, 0, 0)
            .single()
    }

    pub async fn ensure(
        self,
        db: &impl ConnectionTrait,
        month: DateTime<Utc>,
    ) -> Result<(), DbErr> {
        let start = month_start(month);
        let end = add_months(start, 1);
        db.execute_unprepared(&format!(
            "CREATE TABLE IF NOT EXISTS {schema}.{child} PARTITION OF {schema}.{parent} \
             FOR VALUES FROM ('{from}') TO ('{to}')",
            schema = self.schema,
            child = self.partition_name(start),
            parent = self.parent,
            from = start.to_rfc3339(),
            to = end.to_rfc3339(),
        ))
        .await?;
        Ok(())
    }

    pub async fn ensure_open_through(
        self,
        db: &impl ConnectionTrait,
        now: DateTime<Utc>,
        months_ahead: u32,
    ) -> Result<(), DbErr> {
        let start = month_start(now);
        for ahead in 0..=months_ahead {
            self.ensure(db, add_months(start, ahead)).await?;
        }
        Ok(())
    }

    pub async fn drop_older_than(
        self,
        db: &impl ConnectionTrait,
        now: DateTime<Utc>,
        retention_months: u32,
    ) -> Result<Vec<String>, DbErr> {
        let cutoff = subtract_months(month_start(now), retention_months);
        let mut dropped = Vec::new();
        for name in self.names(db).await? {
            let Some(start) = self.month_of(&name) else {
                continue;
            };
            if start < cutoff {
                db.execute_unprepared(&format!("DROP TABLE IF EXISTS {}.{name}", self.schema))
                    .await?;
                dropped.push(name);
            }
        }
        Ok(dropped)
    }

    pub async fn names(self, db: &impl ConnectionTrait) -> Result<Vec<String>, DbErr> {
        let rows = db
            .query_all_raw(Statement::from_string(
                DbBackend::Postgres,
                format!(
                    "SELECT child.relname AS name FROM pg_inherits \
                     JOIN pg_class AS child ON child.oid = pg_inherits.inhrelid \
                     WHERE pg_inherits.inhparent = '{}.{}'::regclass ORDER BY child.relname",
                    self.schema, self.parent
                ),
            ))
            .await?;
        rows.iter().map(|row| row.try_get("", "name")).collect()
    }

    pub async fn partition_key(self, db: &impl ConnectionTrait) -> Result<Option<String>, DbErr> {
        Ok(db
            .query_one_raw(Statement::from_string(
                DbBackend::Postgres,
                format!(
                    "SELECT pg_get_partkeydef('{}.{}'::regclass) AS key",
                    self.schema, self.parent
                ),
            ))
            .await?
            .map(|row| row.try_get("", "key"))
            .transpose()?
            .flatten())
    }

    pub async fn is_partitioned(self, db: &impl ConnectionTrait) -> Result<bool, DbErr> {
        Ok(self
            .partition_key(db)
            .await?
            .is_some_and(|key| key.contains(self.key_column)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EVENTS: Monthly = Monthly::new("chat", "events", "occurred_at");
    const DECISIONS: Monthly = Monthly::new("llm_router", "decisions", "created_at");

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

        assert_eq!(EVENTS.partition_name(start), "events_y2026m12");
        assert_eq!(
            add_months(start, 1),
            at(2027, 1, 1),
            "the upper bound rolls into the next year, and is exclusive"
        );
    }

    #[test]
    fn partition_names_round_trip_through_month_of() {
        for parent in [EVENTS, DECISIONS] {
            for (year, month) in [(2026, 1), (2026, 9), (2026, 12)] {
                let start = at(year, month, 1);
                assert_eq!(parent.month_of(&parent.partition_name(start)), Some(start));
            }
        }
    }

    #[test]
    fn month_of_ignores_tables_this_parent_did_not_name() {
        for name in ["events", "events_backup", "events_y2026m", "sessions"] {
            assert_eq!(
                EVENTS.month_of(name),
                None,
                "{name} must not look like an events partition"
            );
        }
        for name in [
            "decisions",
            "decisions_backup",
            "decisions_y2026m",
            "decision_payloads_y2026m09",
        ] {
            assert_eq!(
                DECISIONS.month_of(name),
                None,
                "{name} must not look like a decisions partition"
            );
        }
    }

    #[test]
    fn a_timestamp_outside_the_open_window_is_pulled_back_into_it() {
        let now = at(2026, 9, 16);

        assert_eq!(clamp_to_open_window(at(2026, 7, 4), now, 1), at(2026, 9, 1));
        assert_eq!(
            clamp_to_open_window(at(2027, 5, 1), now, 1),
            at(2026, 11, 1) - TimeDelta::microseconds(1),
            "the upper bound is the last instant next month's partition accepts"
        );
        let inside = at(2026, 10, 9);
        assert_eq!(clamp_to_open_window(inside, now, 1), inside);
    }
}
