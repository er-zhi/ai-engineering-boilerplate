// The cron daemon: claims one due schedule per poll and starts its execution.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use sea_orm::sea_query::{LockBehavior, LockType};
use sea_orm::{
    ActiveModelTrait,
    ActiveValue::{Set, Unchanged},
    ColumnTrait, DatabaseConnection, DbErr, EntityTrait, QueryFilter, QueryOrder, QuerySelect,
    TransactionTrait,
};
use uuid::Uuid;

use crate::cron_next::next_fire_after;
use crate::entity::schedule;
use crate::service::Service;

#[derive(Debug, Clone, PartialEq)]
struct DueSchedule {
    id: i64,
    graph_id: String,
    graph_version: Option<i32>,
    input_json: serde_json::Value,
    user_id: Option<Uuid>,
}

pub async fn run_forever(db: DatabaseConnection, service: Arc<Service>, poll_interval: Duration) {
    let mut interval = tokio::time::interval(poll_interval);
    loop {
        interval.tick().await;
        run_once(&db, &service).await;
    }
}

async fn run_once(db: &DatabaseConnection, service: &Service) -> Option<Uuid> {
    let due = match claim_one_due(db).await {
        Ok(due) => due?,
        Err(error) => {
            tracing::error!(%error, "claiming a due schedule failed");
            return None;
        }
    };
    fire(db, service, &due).await
}

async fn claim_one_due(db: &DatabaseConnection) -> Result<Option<DueSchedule>, DbErr> {
    let txn = db.begin().await?;
    let Some(row) = schedule::Entity::find()
        .filter(schedule::Column::Enabled.eq(true))
        .filter(schedule::Column::NextRunAt.lte(Utc::now()))
        .order_by_asc(schedule::Column::NextRunAt)
        .lock_with_behavior(LockType::Update, LockBehavior::SkipLocked)
        .one(&txn)
        .await?
    else {
        txn.rollback().await?;
        return Ok(None);
    };

    let next_run_at = match next_fire_after(&row.cron_expr, Utc::now()) {
        Ok(next) => Some(next),
        Err(error) => {
            tracing::error!(
                schedule_id = row.id,
                %error,
                "disabling a schedule whose cron expression cannot be advanced"
            );
            None
        }
    };
    let mut advance = schedule::ActiveModel {
        id: Unchanged(row.id),
        updated_at: Set(Utc::now()),
        ..Default::default()
    };
    match next_run_at {
        Some(next) => advance.next_run_at = Set(next),
        None => advance.enabled = Set(false),
    }
    advance.update(&txn).await?;
    txn.commit().await?;

    Ok(next_run_at.map(|_| DueSchedule {
        id: row.id,
        graph_id: row.graph_id,
        graph_version: row.graph_version,
        input_json: row.input_json,
        user_id: row.user_id,
    }))
}

async fn fire(db: &DatabaseConnection, service: &Service, due: &DueSchedule) -> Option<Uuid> {
    let execution_id = start(service, due).await?;
    record_last_execution(db, due.id, execution_id).await;
    Some(execution_id)
}

async fn start(service: &Service, due: &DueSchedule) -> Option<Uuid> {
    match service
        .start_execution(
            due.graph_id.clone(),
            due.graph_version,
            &due.input_json.to_string(),
            due.user_id,
        )
        .await
    {
        Ok(execution_id) => {
            tracing::info!(
                schedule_id = due.id,
                graph_id = due.graph_id,
                %execution_id,
                "scheduled run started"
            );
            Some(execution_id)
        }
        Err(error) => {
            tracing::error!(
                schedule_id = due.id,
                graph_id = due.graph_id,
                %error,
                "scheduled run failed to start"
            );
            None
        }
    }
}

async fn record_last_execution(db: &DatabaseConnection, schedule_id: i64, execution_id: Uuid) {
    let record = schedule::ActiveModel {
        id: Unchanged(schedule_id),
        last_execution_id: Set(Some(execution_id)),
        updated_at: Set(Utc::now()),
        ..Default::default()
    };
    if let Err(error) = record.update(db).await {
        tracing::warn!(schedule_id, %error, "could not record last_execution_id");
    }
}

#[cfg(all(test, feature = "test-support"))]
mod tests {
    use super::*;

    const HOURLY: &str = "0 0 * * * *";

    async fn seed_schedule(
        db: &DatabaseConnection,
        next_run_at: chrono::DateTime<Utc>,
        cron_expr: &str,
    ) -> i64 {
        schedule::ActiveModel {
            graph_id: Set("simple".to_owned()),
            graph_version: Set(None),
            cron_expr: Set(cron_expr.to_owned()),
            input_json: Set(serde_json::json!({"question": "scheduled"})),
            user_id: Set(None),
            enabled: Set(true),
            next_run_at: Set(next_run_at),
            last_execution_id: Set(None),
            created_at: Set(Utc::now()),
            updated_at: Set(Utc::now()),
            ..Default::default()
        }
        .insert(db)
        .await
        .expect("seed schedule")
        .id
    }

    async fn reload(db: &DatabaseConnection, id: i64) -> schedule::Model {
        schedule::Entity::find_by_id(id)
            .one(db)
            .await
            .expect("query")
            .expect("row")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_due_schedule_is_claimed_and_its_next_run_advances() {
        let test = crate::test_db::start().await;
        let due_at = Utc::now() - chrono::Duration::minutes(5);
        let id = seed_schedule(&test.db, due_at, HOURLY).await;

        let claimed = claim_one_due(&test.db)
            .await
            .expect("claim")
            .expect("a due schedule");

        assert_eq!(claimed.id, id);
        assert_eq!(claimed.graph_id, "simple");
        assert_eq!(
            claimed.input_json,
            serde_json::json!({"question": "scheduled"})
        );
        let row = reload(&test.db, id).await;
        assert!(
            row.next_run_at > due_at,
            "next_run_at advanced past the claimed fire time: {} vs {due_at}",
            row.next_run_at
        );
        assert!(row.next_run_at > Utc::now(), "and into the future");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_schedule_due_in_the_future_is_not_claimed() {
        let test = crate::test_db::start().await;
        let next_run_at = Utc::now() + chrono::Duration::hours(2);
        let id = seed_schedule(&test.db, next_run_at, HOURLY).await;

        assert_eq!(claim_one_due(&test.db).await.expect("claim"), None);
        assert_eq!(
            reload(&test.db, id).await.next_run_at,
            next_run_at,
            "an untouched schedule keeps its fire time"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_second_claim_does_not_fire_the_same_schedule_twice() {
        let test = crate::test_db::start().await;
        seed_schedule(&test.db, Utc::now() - chrono::Duration::minutes(5), HOURLY).await;

        let first = claim_one_due(&test.db).await.expect("first claim");
        let second = claim_one_due(&test.db).await.expect("second claim");

        assert!(first.is_some(), "the first claim takes the due schedule");
        assert_eq!(
            second, None,
            "the claim advanced next_run_at in the same transaction, so a second worker sees nothing due"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_claimed_schedule_starts_a_real_execution_and_records_it() {
        let test = crate::test_db::start().await;
        let service = Service::new(test.db.clone());
        service
            .register_graph(
                "simple".to_owned(),
                &serde_json::to_string(&engine_core::simple_graph()).expect("serialize"),
            )
            .await
            .expect("register");
        let id = seed_schedule(&test.db, Utc::now() - chrono::Duration::minutes(5), HOURLY).await;

        let execution_id = run_once(&test.db, &service)
            .await
            .expect("the due schedule started an execution");

        let execution = service.get_execution(execution_id).await.expect("get");
        assert_eq!(
            execution.state,
            serde_json::json!({"question": "scheduled"})
        );
        assert_eq!(
            reload(&test.db, id).await.last_execution_id,
            Some(execution_id)
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_schedule_whose_expression_no_longer_parses_is_disabled_not_spun_on() {
        let test = crate::test_db::start().await;
        let id = seed_schedule(
            &test.db,
            Utc::now() - chrono::Duration::minutes(5),
            "not a cron expression",
        )
        .await;

        assert_eq!(
            claim_one_due(&test.db).await.expect("claim"),
            None,
            "an unfireable schedule is not handed to the executor"
        );
        let row = reload(&test.db, id).await;
        assert!(
            !row.enabled,
            "and it is disabled, so the loop doesn't re-claim it every poll forever"
        );
    }
}
