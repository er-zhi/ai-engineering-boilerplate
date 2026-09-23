// Creates the four llm_router audit tables and keeps their monthly partitions: opens the months ahead under one advisory lock, and drops the payload months past retention, which a DROP can only promise at month granularity — a payload written in month M leaves once M+2 opens, so it lives at least the length of M+1 and at most len(M) + len(M+1).

use chrono::{DateTime, Utc};
use common::partition::Monthly;
use sea_orm::{ConnectionTrait, DatabaseConnection, DbErr, EntityName, TransactionTrait};

use crate::audit::{decision, decision_payload, payload, request};

const SCHEMA: &str = "llm_router";
const PARTITION_KEY_COLUMN: &str = "created_at";
const MONTHS_OPEN_AHEAD: u32 = 1;
const PAYLOAD_RETENTION_MONTHS: u32 = 1;
const MAINTENANCE_LOCK: i64 = 0x6c6c_6d72_7061_7274;

fn parent_of(entity: impl EntityName) -> Monthly {
    Monthly::new(SCHEMA, entity.table_name(), PARTITION_KEY_COLUMN)
}

fn parents() -> [Monthly; 4] {
    [
        parent_of(request::Entity),
        parent_of(payload::Entity),
        parent_of(decision::Entity),
        parent_of(decision_payload::Entity),
    ]
}

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
        parent
            .ensure_open_through(&pass, now, MONTHS_OPEN_AHEAD)
            .await?;
    }
    let swept = Swept {
        request_payloads: parent_of(payload::Entity)
            .drop_older_than(&pass, now, PAYLOAD_RETENTION_MONTHS)
            .await?,
        decision_payloads: parent_of(decision_payload::Entity)
            .drop_older_than(&pass, now, PAYLOAD_RETENTION_MONTHS)
            .await?,
    };
    pass.commit().await?;
    Ok(swept)
}

async fn alone(db: &DatabaseConnection) -> Result<sea_orm::DatabaseTransaction, DbErr> {
    let pass = db.begin().await?;
    pass.execute_unprepared(&format!("SELECT pg_advisory_xact_lock({MAINTENANCE_LOCK})"))
        .await?;
    Ok(pass)
}

pub async fn plain_parents(db: &impl ConnectionTrait) -> Result<Vec<&'static str>, DbErr> {
    let mut plain = Vec::new();
    for parent in parents() {
        if !parent.is_partitioned(db).await? {
            plain.push(parent.parent());
        }
    }
    Ok(plain)
}

#[cfg(test)]
mod tests {
    use sea_orm::ActiveValue::Set;
    use sea_orm::{ActiveModelTrait, EntityTrait};

    use super::*;
    use crate::test_db;
    use chrono::TimeZone;
    use common::partition::{add_months, month_start, subtract_months};

    fn at(year: i32, month: u32, day: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, 0, 0, 0)
            .single()
            .expect("a real instant")
    }

    #[test]
    fn a_payload_is_kept_for_at_least_the_month_it_landed_in_and_the_one_before() {
        let payloads = parent_of(decision_payload::Entity);
        let cutoff = subtract_months(month_start(at(2026, 11, 1)), PAYLOAD_RETENTION_MONTHS);

        assert_eq!(cutoff, at(2026, 10, 1));
        assert!(
            payloads.month_of("decision_payloads_y2026m09") < Some(cutoff),
            "a payload from September is gone once November opens, 31 days after the month closed"
        );
        assert!(
            payloads.month_of("decision_payloads_y2026m10") >= Some(cutoff),
            "a payload from October is still within the month partition retention keeps"
        );
    }

    #[test]
    fn the_shortest_retention_is_the_length_of_february() {
        let payloads = parent_of(decision_payload::Entity);
        let march = subtract_months(month_start(at(2026, 3, 1)), PAYLOAD_RETENTION_MONTHS);

        assert_eq!(march, at(2026, 2, 1));
        assert!(
            payloads.month_of("decision_payloads_y2026m01") < Some(march),
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
                parent.partition_key(&test.db).await.unwrap().as_deref(),
                Some("RANGE (created_at)"),
                "{} must be the partitioned parent, not a plain table",
                parent.parent()
            );
        }
    }

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
            parent.ensure(&test.db, long_ago).await.unwrap();
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
                parent
                    .names(&test.db)
                    .await
                    .unwrap()
                    .contains(&parent.partition_name(next)),
                "{} must have next month open before the month turns",
                parent.parent()
            );
        }
    }
}
