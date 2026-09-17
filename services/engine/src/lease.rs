// Claims and releases an execution's lease.

use chrono::Utc;
use sea_orm::{
    ActiveModelTrait,
    ActiveValue::{Set, Unchanged},
    ColumnTrait, Condition, ConnectionTrait, DatabaseBackend, DatabaseConnection,
    DatabaseTransaction, DbErr, EntityTrait, QueryFilter, QueryOrder, QuerySelect, Statement,
    TransactionTrait,
    sea_query::{Expr, LockBehavior, LockType},
};
use std::time::Duration;
use uuid::Uuid;

use crate::entity::execution;

const FALLBACK_LEASE: chrono::TimeDelta = chrono::TimeDelta::seconds(30);

const ELAPSED_TIMER_WAIT: &str =
    "wait_kind ? 'Timer' AND (wait_kind -> 'Timer' ->> 'until')::timestamptz <= $1";

pub async fn claim_one_ready(
    db: &DatabaseConnection,
    owner: &str,
    lease_for: Duration,
) -> Result<Option<Uuid>, DbErr> {
    let now = Utc::now();
    let lease_until = now + chrono::Duration::from_std(lease_for).unwrap_or(FALLBACK_LEASE);

    let txn = db.begin().await?;
    let Some(row) = execution::Entity::find()
        .filter(claimable(now))
        .order_by_asc(execution::Column::UpdatedAt)
        .lock_with_behavior(LockType::Update, LockBehavior::SkipLocked)
        .one(&txn)
        .await?
    else {
        txn.rollback().await?;
        return Ok(None);
    };

    execution::ActiveModel {
        id: Unchanged(row.id),
        status: Set(execution::Status::Running),
        lease_owner: Set(Some(owner.to_owned())),
        lease_until: Set(Some(lease_until)),
        updated_at: Set(now),
        ..Default::default()
    }
    .update(&txn)
    .await?;
    txn.commit().await?;
    Ok(Some(row.id))
}

fn claimable(now: chrono::DateTime<Utc>) -> Condition {
    Condition::any()
        .add(execution::Column::Status.eq(execution::Status::Ready))
        .add(
            Condition::all()
                .add(execution::Column::Status.eq(execution::Status::Running))
                .add(execution::Column::LeaseUntil.lt(now)),
        )
        .add(
            Condition::all()
                .add(execution::Column::Status.eq(execution::Status::Waiting))
                .add(Expr::cust_with_values(ELAPSED_TIMER_WAIT, [now])),
        )
}

const SOLE_ENGINE_LOCK_KEY: i64 = 8085;

pub async fn claim_sole_engine_lock(
    db: &DatabaseConnection,
) -> Result<Option<DatabaseTransaction>, DbErr> {
    let txn = db.begin().await?;
    let taken = txn
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            format!("SELECT pg_try_advisory_xact_lock({SOLE_ENGINE_LOCK_KEY}) AS taken"),
        ))
        .await?
        .ok_or_else(|| DbErr::Custom("the advisory lock query returned no row".to_owned()))?
        .try_get::<bool>("", "taken")?;
    if !taken {
        txn.rollback().await?;
        return Ok(None);
    }
    Ok(Some(txn))
}

pub async fn expire_abandoned_leases(db: &impl ConnectionTrait) -> Result<u64, DbErr> {
    let now = Utc::now();
    let result = execution::Entity::update_many()
        .set(execution::ActiveModel {
            lease_until: Set(Some(now)),
            ..Default::default()
        })
        .filter(execution::Column::Status.eq(execution::Status::Running))
        .filter(execution::Column::LeaseUntil.gt(now))
        .exec(db)
        .await?;
    Ok(result.rows_affected)
}

pub async fn release_lease(
    db: &impl ConnectionTrait,
    execution_id: Uuid,
    owner: Option<&str>,
    status: execution::Status,
    wait_kind: Option<serde_json::Value>,
    current_nodes: serde_json::Value,
) -> Result<u64, DbErr> {
    let result = execution::Entity::update_many()
        .set(execution::ActiveModel {
            status: Set(status),
            wait_kind: Set(wait_kind),
            current_nodes: Set(current_nodes),
            lease_owner: Set(None),
            lease_until: Set(None),
            updated_at: Set(Utc::now()),
            ..Default::default()
        })
        .filter(execution::Column::Id.eq(execution_id))
        .filter(match owner {
            Some(owner) => execution::Column::LeaseOwner.eq(owner),
            None => execution::Column::LeaseOwner.is_null(),
        })
        .exec(db)
        .await?;
    Ok(result.rows_affected)
}

#[cfg(all(test, feature = "test-support"))]
mod tests {
    use super::*;
    use std::time::Duration;

    async fn seed_ready_execution(db: &DatabaseConnection) -> Uuid {
        let id = Uuid::new_v4();
        crate::entity::execution::ActiveModel {
            id: Set(id),
            graph_id: Set("agent".to_owned()),
            graph_version: Set(1),
            user_id: Set(None),
            status: Set(execution::Status::Ready),
            wait_kind: Set(None),
            current_nodes: Set(serde_json::json!([])),
            iteration: Set(0),
            max_iterations: Set(10),
            deadline: Set(None),
            budget: Set(serde_json::json!({})),
            lease_owner: Set(None),
            lease_until: Set(None),
            created_at: Set(chrono::Utc::now()),
            updated_at: Set(chrono::Utc::now()),
        }
        .insert(db)
        .await
        .expect("seed");
        id
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn claim_takes_a_ready_execution_and_marks_it_running() {
        let test = crate::test_db::start().await;
        let id = seed_ready_execution(&test.db).await;

        let claimed = claim_one_ready(&test.db, "worker-1", Duration::from_secs(30))
            .await
            .expect("claim")
            .expect("something to claim");

        assert_eq!(claimed, id);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_second_claim_attempt_skips_an_already_leased_row() {
        let test = crate::test_db::start().await;
        seed_ready_execution(&test.db).await;

        claim_one_ready(&test.db, "worker-1", Duration::from_secs(30))
            .await
            .expect("first claim")
            .expect("claimed");
        let second = claim_one_ready(&test.db, "worker-2", Duration::from_secs(30))
            .await
            .expect("second claim attempt");

        assert_eq!(
            second, None,
            "nothing else ready — the row is leased, not reclaimable yet"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_expired_lease_is_reclaimed() {
        let test = crate::test_db::start().await;
        let id = seed_ready_execution(&test.db).await;
        claim_one_ready(&test.db, "worker-1", Duration::from_millis(1))
            .await
            .expect("claim")
            .expect("claimed");
        tokio::time::sleep(Duration::from_millis(20)).await;

        let reclaimed = claim_one_ready(&test.db, "worker-2", Duration::from_secs(30))
            .await
            .expect("reclaim")
            .expect("something reclaimable");

        assert_eq!(
            reclaimed, id,
            "worker-1's lease expired, worker-2 picks it up"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn startup_recovery_frees_an_execution_a_replaced_engine_left_running() {
        let test = crate::test_db::start().await;
        let id = seed_ready_execution(&test.db).await;
        claim_one_ready(
            &test.db,
            "engine-before-the-restart",
            Duration::from_secs(300),
        )
        .await
        .expect("claim")
        .expect("claimed");
        assert_eq!(
            claim_one_ready(
                &test.db,
                "engine-after-the-restart",
                Duration::from_secs(300)
            )
            .await
            .expect("claim attempt"),
            None,
            "without recovery the fresh process waits out the whole 5-minute lease"
        );

        let freed = expire_abandoned_leases(&test.db).await.expect("recover");

        assert_eq!(freed, 1);
        let reclaimed = claim_one_ready(
            &test.db,
            "engine-after-the-restart",
            Duration::from_secs(300),
        )
        .await
        .expect("reclaim")
        .expect("the orphaned execution is claimable again");
        assert_eq!(reclaimed, id);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn startup_recovery_leaves_executions_that_are_not_running_alone() {
        let test = crate::test_db::start().await;
        seed_ready_execution(&test.db).await;

        let freed = expire_abandoned_leases(&test.db).await.expect("recover");

        assert_eq!(freed, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn release_lease_clears_ownership_and_sets_the_new_status() {
        let test = crate::test_db::start().await;
        let id = seed_ready_execution(&test.db).await;
        claim_one_ready(&test.db, "worker-1", Duration::from_secs(30))
            .await
            .expect("claim");

        let released = release_lease(
            &test.db,
            id,
            Some("worker-1"),
            execution::Status::Completed,
            None,
            serde_json::json!([]),
        )
        .await
        .expect("release");

        assert_eq!(released, 1);
        let row = crate::entity::execution::Entity::find_by_id(id)
            .one(&test.db)
            .await
            .expect("query")
            .expect("row still there");
        assert_eq!(row.status, execution::Status::Completed);
        assert_eq!(row.lease_owner, None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_release_by_a_worker_that_no_longer_holds_the_lease_changes_nothing() {
        let test = crate::test_db::start().await;
        let id = seed_ready_execution(&test.db).await;
        claim_one_ready(&test.db, "worker-1", Duration::from_millis(1))
            .await
            .expect("claim")
            .expect("claimed");
        tokio::time::sleep(Duration::from_millis(20)).await;
        claim_one_ready(&test.db, "worker-2", Duration::from_secs(30))
            .await
            .expect("reclaim")
            .expect("reclaimed");

        let released = release_lease(
            &test.db,
            id,
            Some("worker-1"),
            execution::Status::Completed,
            None,
            serde_json::json!([]),
        )
        .await
        .expect("release");

        assert_eq!(released, 0);
        let row = crate::entity::execution::Entity::find_by_id(id)
            .one(&test.db)
            .await
            .expect("query")
            .expect("row still there");
        assert_eq!(row.status, execution::Status::Running);
        assert_eq!(row.lease_owner.as_deref(), Some("worker-2"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_release_of_an_unleased_execution_needs_no_owner() {
        let test = crate::test_db::start().await;
        let id = seed_ready_execution(&test.db).await;

        let released = release_lease(
            &test.db,
            id,
            None,
            execution::Status::Cancelled,
            None,
            serde_json::json!([]),
        )
        .await
        .expect("release");

        assert_eq!(released, 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_second_engine_process_cannot_take_the_sole_engine_lock() {
        let test = crate::test_db::start().await;
        let first = claim_sole_engine_lock(&test.db)
            .await
            .expect("lock attempt")
            .expect("nothing else holds it");

        assert!(
            claim_sole_engine_lock(&test.db)
                .await
                .expect("lock attempt")
                .is_none()
        );

        drop(first);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_elapsed_timer_wait_is_claimable_and_an_unelapsed_one_is_not() {
        let test = crate::test_db::start().await;
        let elapsed = seed_ready_execution(&test.db).await;
        let pending = seed_ready_execution(&test.db).await;
        let park = |id: Uuid, until: chrono::DateTime<Utc>| execution::ActiveModel {
            id: Unchanged(id),
            status: Set(execution::Status::Waiting),
            wait_kind: Set(Some(
                serde_json::to_value(engine_core::WaitKind::Timer { until }).expect("serialize"),
            )),
            ..Default::default()
        };
        park(elapsed, Utc::now() - chrono::TimeDelta::minutes(1))
            .update(&test.db)
            .await
            .expect("park elapsed");
        park(pending, Utc::now() + chrono::TimeDelta::hours(1))
            .update(&test.db)
            .await
            .expect("park pending");

        let claimed = claim_one_ready(&test.db, "worker-1", Duration::from_secs(30))
            .await
            .expect("claim")
            .expect("the elapsed timer is due");
        assert_eq!(claimed, elapsed);

        assert_eq!(
            claim_one_ready(&test.db, "worker-2", Duration::from_secs(30))
                .await
                .expect("second claim"),
            None,
            "the timer that has not elapsed stays parked"
        );
    }
}
