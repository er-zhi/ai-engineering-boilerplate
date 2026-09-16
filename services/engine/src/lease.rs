// Claiming and releasing an execution's lease: the only place engine touches
// FOR UPDATE SKIP LOCKED. A single UPDATE...WHERE id = (SELECT ... FOR UPDATE SKIP LOCKED LIMIT
// 1)...RETURNING claims a fresh Ready row, one whose lease expired (its owner crashed mid-tick),
// or one parked on a WaitKind::Timer whose `until` has passed — one query serves "new work",
// "crash recovery" and "the clock is the external event", no separate sweep and no timer daemon.

use chrono::Utc;
use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, DbErr, Statement};
use std::time::Duration;
use uuid::Uuid;

/// Claims one execution for this worker, or `None` when nothing is due.
///
/// The third `WHERE` disjunct is what makes `WaitKind::Timer` actually tick: `wait_kind` is jsonb
/// holding `WaitKind`'s plain externally tagged serde shape (`{"Timer": {"until": "<rfc3339>"}}`),
/// so "this timer elapsed" is a jsonb path plus a `timestamptz` cast. The claim deliberately
/// leaves `wait_kind` untouched — the tick that picks the row up re-runs its `current_nodes` (still
/// the `Wait` node) with a synthetic `Ok(Value::Null)` output, and `step()`'s ordinary per-output
/// edge evaluation walks past the `Wait` node, which is the same unparking path `resume()` already
/// relies on. `release_lease` then overwrites `status`/`wait_kind` at commit time.
pub async fn claim_one_ready(
    db: &DatabaseConnection,
    owner: &str,
    lease_for: Duration,
) -> Result<Option<Uuid>, DbErr> {
    let lease_until = Utc::now()
        + chrono::Duration::from_std(lease_for).unwrap_or_else(|_| chrono::Duration::seconds(30));
    let stmt = Statement::from_sql_and_values(
        DbBackend::Postgres,
        r"
        UPDATE engine.executions
        SET status = 'running', lease_owner = $1, lease_until = $2, updated_at = now()
        WHERE id = (
            SELECT id FROM engine.executions
            WHERE status = 'ready'
               OR (status = 'running' AND lease_until < now())
               -- a Waiting(Timer) row whose until has passed (see the doc comment above)
               OR (
                   status = 'waiting'
                   AND wait_kind ? 'Timer'
                   AND (wait_kind -> 'Timer' ->> 'until')::timestamptz <= now()
               )
            ORDER BY updated_at
            FOR UPDATE SKIP LOCKED
            LIMIT 1
        )
        RETURNING id
        ",
        [owner.into(), lease_until.into()],
    );
    let row = db.query_one_raw(stmt).await?;
    row.map(|r| r.try_get("", "id")).transpose()
}

/// Makes every execution left `running` by a previous engine process claimable again, returning
/// how many were freed. Call once at startup, before the tick loop starts.
///
/// `claim_one_ready` already reclaims an abandoned lease — but only once it expires, and
/// `LEASE_DURATION` is 5 minutes (it has to cover a worst-case tick, nothing renews it mid-tick).
/// A container replaced mid-tick therefore leaves its execution frozen for those 5 minutes, and
/// anything serialized behind it (Chat's topic, `Interrupt`, `Resume`) blocks with "is mid-tick,
/// retry shortly" the whole time. Expiring the lease at startup hands the row straight back to
/// the ordinary claim path, which resumes it from its last checkpoint — no execution is failed
/// for having been interrupted, because nothing about it was lost.
///
/// This assumes the process that just started owns no running execution and no *other* engine
/// process is mid-tick: true for this stack, which runs a single engine replica (`compose.yaml`).
/// Were a second replica added, this would need to skip leases still held by a live peer — a
/// worker registry or a heartbeat-renewed short lease, neither of which exists yet.
pub async fn expire_abandoned_leases(db: &impl ConnectionTrait) -> Result<u64, DbErr> {
    let result = db
        .execute_raw(Statement::from_string(
            DbBackend::Postgres,
            r"
            UPDATE engine.executions
            SET lease_until = now()
            WHERE status = 'running' AND lease_until > now()
            ",
        ))
        .await?;
    Ok(result.rows_affected())
}

/// Takes `&impl ConnectionTrait`, not `&DatabaseConnection` specifically — Task 15's tick commit
/// calls this with a `&DatabaseTransaction` (also `ConnectionTrait`) so the status/wait_kind/
/// current_nodes update lands in the same transaction as that tick's checkpoint and events; this
/// task's own tests below call it with `&test.db` (a `DatabaseConnection`), which implements the
/// same trait, so they keep compiling unchanged.
pub async fn release_lease(
    db: &impl ConnectionTrait,
    execution_id: Uuid,
    status: &str,
    wait_kind: Option<serde_json::Value>,
    current_nodes: serde_json::Value,
) -> Result<(), DbErr> {
    let stmt = Statement::from_sql_and_values(
        DbBackend::Postgres,
        r"
        UPDATE engine.executions
        SET status = $2, wait_kind = $3, current_nodes = $4, lease_owner = NULL,
            lease_until = NULL, updated_at = now()
        WHERE id = $1
        ",
        [
            execution_id.into(),
            status.into(),
            wait_kind.into(),
            current_nodes.into(),
        ],
    );
    db.execute_raw(stmt).await?;
    Ok(())
}

#[cfg(all(test, feature = "test-support"))]
mod tests {
    use super::*;
    use sea_orm::{ActiveModelTrait, ActiveValue::Set, EntityTrait};
    use std::time::Duration;

    async fn seed_ready_execution(db: &DatabaseConnection) -> Uuid {
        let id = Uuid::new_v4();
        crate::entity::execution::ActiveModel {
            id: Set(id),
            graph_id: Set("agent".to_owned()),
            graph_version: Set(1),
            user_id: Set(None),
            status: Set("ready".to_owned()),
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
        // A previous process claimed it with a long lease and then died mid-tick.
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

        release_lease(&test.db, id, "completed", None, serde_json::json!([]))
            .await
            .expect("release");

        let row = crate::entity::execution::Entity::find_by_id(id)
            .one(&test.db)
            .await
            .expect("query")
            .expect("row still there");
        assert_eq!(row.status, "completed");
        assert_eq!(row.lease_owner, None);
    }
}
