// Creates the four llm_router audit tables and keeps their monthly partitions: opens the months ahead, drops the payload months past retention.

use chrono::{DateTime, Datelike, Months, TimeZone, Utc};
use sea_orm::{
    ConnectionTrait, DatabaseConnection, DbBackend, DbErr, EntityName, Statement, TransactionTrait,
};

use crate::audit::{decision, decision_payload, payload, request};

const SCHEMA: &str = "llm_router";
const MONTHS_OPEN_AHEAD: u32 = 1;
// A month is the granularity a partition drop works at, so "keep a payload for 30 days" becomes "keep the
// month it landed in, and the month before the current one": a payload written in month M is dropped once
// M+2 opens, so it survives at least the length of M+1 — 28 days when that is February, 29 in a leap year —
// and at most len(M) + len(M+1), which is 62. A finer promise would need finer partitions than a DROP.
const PAYLOAD_RETENTION_MONTHS: u32 = 1;
// `CREATE TABLE IF NOT EXISTS ... PARTITION OF` raises a duplicate object rather than skipping when two
// sessions race it, and a rolling restart briefly runs two containers. One transaction-scoped advisory lock
// serializes the whole pass; it rides that transaction's own connection and is released when it ends.
const MAINTENANCE_LOCK: i64 = 0x6c6c_6d72_7061_7274;

// The four parents, named by the entities themselves so a table rename cannot leave this pass pointing at a
// table that no longer exists.
fn parents() -> [&'static str; 4] {
    [
        request::Entity.table_name(),
        payload::Entity.table_name(),
        decision::Entity.table_name(),
        decision_payload::Entity.table_name(),
    ]
}

// What one sweep dropped, kept apart by class so an operator can see which payloads are actually aging out.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Swept {
    pub request_payloads: Vec<String>,
    pub decision_payloads: Vec<String>,
}

impl Swept {
    pub fn is_empty(&self) -> bool {
        self.request_payloads.is_empty() && self.decision_payloads.is_empty()
    }
}

pub async fn create_parents(db: &DatabaseConnection) -> Result<(), DbErr> {
    let pass = alone(db).await?;
    for statement in [
        request::TABLE_STATEMENT,
        payload::TABLE_STATEMENT,
        decision::TABLE_STATEMENT,
        decision_payload::TABLE_STATEMENT,
    ] {
        pass.execute_unprepared(statement).await?;
    }
    pass.commit().await
}

pub async fn maintain(db: &DatabaseConnection, now: DateTime<Utc>) -> Result<Swept, DbErr> {
    let pass = alone(db).await?;
    for parent in parents() {
        ensure_partition(&pass, parent, now).await?;
        ensure_partition(
            &pass,
            parent,
            add_months(month_start(now), MONTHS_OPEN_AHEAD),
        )
        .await?;
    }
    let swept = Swept {
        request_payloads: drop_partitions_past_retention(&pass, payload::Entity.table_name(), now)
            .await?,
        decision_payloads: drop_partitions_past_retention(
            &pass,
            decision_payload::Entity.table_name(),
            now,
        )
        .await?,
    };
    pass.commit().await?;
    Ok(swept)
}

// One transaction holding the advisory lock, so no second container is inside this DDL at the same time.
async fn alone(db: &DatabaseConnection) -> Result<sea_orm::DatabaseTransaction, DbErr> {
    let pass = db.begin().await?;
    pass.execute_unprepared(&format!("SELECT pg_advisory_xact_lock({MAINTENANCE_LOCK})"))
        .await?;
    Ok(pass)
}

// The parents that came up as plain tables, against which the partitioned CREATE TABLE was a silent no-op.
pub async fn plain_parents(db: &impl ConnectionTrait) -> Result<Vec<&'static str>, DbErr> {
    let mut plain = Vec::new();
    for parent in parents() {
        let key = partition_key(db, parent).await?;
        if !key.is_some_and(|key| key.contains("created_at")) {
            plain.push(parent);
        }
    }
    Ok(plain)
}

async fn ensure_partition(
    db: &impl ConnectionTrait,
    parent: &str,
    month: DateTime<Utc>,
) -> Result<(), DbErr> {
    let start = month_start(month);
    let end = add_months(start, 1);
    db.execute_unprepared(&format!(
        "CREATE TABLE IF NOT EXISTS {SCHEMA}.{} PARTITION OF {SCHEMA}.{parent} \
         FOR VALUES FROM ('{}') TO ('{}')",
        partition_name(parent, start),
        start.to_rfc3339(),
        end.to_rfc3339(),
    ))
    .await?;
    Ok(())
}

async fn drop_partitions_past_retention(
    db: &impl ConnectionTrait,
    parent: &str,
    now: DateTime<Utc>,
) -> Result<Vec<String>, DbErr> {
    let cutoff = subtract_months(month_start(now), PAYLOAD_RETENTION_MONTHS);
    let mut dropped = Vec::new();
    for name in partition_names(db, parent).await? {
        let Some(start) = month_of(parent, &name) else {
            continue;
        };
        if start < cutoff {
            db.execute_unprepared(&format!("DROP TABLE IF EXISTS {SCHEMA}.{name}"))
                .await?;
            dropped.push(name);
        }
    }
    Ok(dropped)
}

// What psql prints as "Partition key:", or None for a plain table.
async fn partition_key(db: &impl ConnectionTrait, parent: &str) -> Result<Option<String>, DbErr> {
    Ok(db
        .query_one_raw(Statement::from_string(
            DbBackend::Postgres,
            format!("SELECT pg_get_partkeydef('{SCHEMA}.{parent}'::regclass) AS key"),
        ))
        .await?
        .map(|row| row.try_get("", "key"))
        .transpose()?
        .flatten())
}

async fn partition_names(db: &impl ConnectionTrait, parent: &str) -> Result<Vec<String>, DbErr> {
    let rows = db
        .query_all_raw(Statement::from_string(
            DbBackend::Postgres,
            format!(
                "SELECT child.relname AS name FROM pg_inherits \
                 JOIN pg_class AS child ON child.oid = pg_inherits.inhrelid \
                 WHERE pg_inherits.inhparent = '{SCHEMA}.{parent}'::regclass ORDER BY child.relname"
            ),
        ))
        .await?;
    rows.iter().map(|row| row.try_get("", "name")).collect()
}

fn partition_name(parent: &str, month_start: DateTime<Utc>) -> String {
    format!(
        "{parent}_y{:04}m{:02}",
        month_start.year(),
        month_start.month()
    )
}

fn month_of(parent: &str, name: &str) -> Option<DateTime<Utc>> {
    let rest = name.strip_prefix(&format!("{parent}_y"))?;
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
    use sea_orm::ActiveValue::Set;
    use sea_orm::{ActiveModelTrait, EntityTrait};

    use super::*;
    use crate::test_db;

    fn at(year: i32, month: u32, day: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, 0, 0, 0)
            .single()
            .expect("a real instant")
    }

    #[test]
    fn partition_names_round_trip_through_month_of() {
        for parent in parents() {
            for (year, month) in [(2026, 1), (2026, 9), (2026, 12)] {
                let start = at(year, month, 1);
                assert_eq!(
                    month_of(parent, &partition_name(parent, start)),
                    Some(start)
                );
            }
        }
    }

    #[test]
    fn month_of_ignores_tables_this_module_did_not_name() {
        for name in [
            "decisions",
            "decisions_backup",
            "decisions_y2026m",
            "decision_payloads_y2026m09",
        ] {
            assert_eq!(
                month_of(decision::Entity.table_name(), name),
                None,
                "{name} must not look like a decisions partition"
            );
        }
    }

    #[test]
    fn a_partition_spans_exactly_its_month() {
        let start = month_start(at(2026, 12, 31));

        assert_eq!(
            partition_name(decision_payload::Entity.table_name(), start),
            "decision_payloads_y2026m12"
        );
        assert_eq!(
            add_months(start, 1),
            at(2027, 1, 1),
            "the upper bound rolls into the next year, and is exclusive"
        );
    }

    #[test]
    fn a_payload_is_kept_for_at_least_the_month_it_landed_in_and_the_one_before() {
        let payloads = decision_payload::Entity.table_name();
        let cutoff = subtract_months(month_start(at(2026, 11, 1)), PAYLOAD_RETENTION_MONTHS);

        assert_eq!(cutoff, at(2026, 10, 1));
        assert!(
            month_of(payloads, "decision_payloads_y2026m09") < Some(cutoff),
            "a payload from September is gone once November opens, 31 days after the month closed"
        );
        assert!(
            month_of(payloads, "decision_payloads_y2026m10") >= Some(cutoff),
            "a payload from October is still within the month partition retention keeps"
        );
    }

    // The shortest a payload can live: written on the last day of January, dropped the moment March opens,
    // which is the 28 days of February and nothing more. The longest is len(M) + len(M+1), here 62.
    #[test]
    fn the_shortest_retention_is_the_length_of_february() {
        let payloads = decision_payload::Entity.table_name();
        let march = subtract_months(month_start(at(2026, 3, 1)), PAYLOAD_RETENTION_MONTHS);

        assert_eq!(march, at(2026, 2, 1));
        assert!(
            month_of(payloads, "decision_payloads_y2026m01") < Some(march),
            "a payload written on 31 January is dropped as March opens, 28 days later"
        );
        assert_eq!(
            (at(2026, 3, 1) - at(2026, 2, 1)).num_days(),
            28,
            "the intervening month is the whole of the minimum"
        );
        assert_eq!(
            (at(2025, 3, 1) - at(2025, 1, 1)).num_days(),
            59,
            "a payload written on 1 January survives January and February"
        );
        assert_eq!(
            (at(2026, 9, 1) - at(2026, 7, 1)).num_days(),
            62,
            "and the longest any payload lives is a 31-day month followed by another"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn every_audit_table_comes_up_partitioned_by_the_month_it_was_written_in() {
        let test = test_db::start().await;

        assert_eq!(plain_parents(&test.db).await.unwrap(), Vec::<&str>::new());
        for parent in parents() {
            assert_eq!(
                partition_key(&test.db, parent).await.unwrap().as_deref(),
                Some("RANGE (created_at)"),
                "{parent} must be the partitioned parent, not a plain table"
            );
        }
    }

    // Proving the check that stops startup: a parent left over as a plain table from before partitioning
    // swallows the partitioned CREATE TABLE, and there would be no DROP PARTITION retention path at all.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_parent_that_came_up_as_a_plain_table_is_named() {
        let test = test_db::start().await;
        let plain = request::Entity.table_name();
        test.db
            .execute_unprepared(&format!(
                "DROP TABLE {SCHEMA}.{plain} CASCADE; \
                 CREATE TABLE {SCHEMA}.{plain} (id bigserial PRIMARY KEY, created_at timestamptz)"
            ))
            .await
            .unwrap();

        assert_eq!(plain_parents(&test.db).await.unwrap(), vec![plain]);
        create_parents(&test.db).await.unwrap();
        assert_eq!(
            plain_parents(&test.db).await.unwrap(),
            vec![plain],
            "the partitioned CREATE TABLE is a silent no-op against it"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn maintenance_opens_the_months_ahead_and_drops_only_the_payload_months_past_retention() {
        let test = test_db::start().await;
        let long_ago = at(2024, 3, 15);
        for parent in parents() {
            ensure_partition(&test.db, parent, long_ago).await.unwrap();
        }
        let aged = decision::ActiveModel {
            model_used: Set("jev-1.13.0".to_owned()),
            questions: Set(1),
            tokens_in: Set(312),
            tokens_out: Set(48),
            latency_ms: Set(274),
            outcome: Set(crate::audit::request::Outcome::Answered),
            created_at: Set(long_ago),
            ..Default::default()
        }
        .insert(&test.db)
        .await
        .unwrap();
        decision_payload::ActiveModel {
            decision_id: Set(aged.id),
            sent: Set(serde_json::json!({"model": "jev-latest"})),
            received: Set(serde_json::json!({"answers": {}})),
            created_at: Set(long_ago),
            ..Default::default()
        }
        .insert(&test.db)
        .await
        .unwrap();

        let swept = maintain(&test.db, Utc::now()).await.unwrap();

        assert_eq!(swept.request_payloads, ["request_payloads_y2024m03"]);
        assert_eq!(swept.decision_payloads, ["decision_payloads_y2024m03"]);
        assert!(
            decision_payload::Entity::find()
                .all(&test.db)
                .await
                .unwrap()
                .is_empty(),
            "the payload left with the partition that held it"
        );
        assert_eq!(
            decision::Entity::find().all(&test.db).await.unwrap().len(),
            1,
            "the statistics keep every month they were written in"
        );
        let next = add_months(month_start(Utc::now()), MONTHS_OPEN_AHEAD);
        for parent in parents() {
            assert!(
                partition_names(&test.db, parent)
                    .await
                    .unwrap()
                    .contains(&partition_name(parent, next)),
                "{parent} must have next month open before the month turns"
            );
        }
    }
}
