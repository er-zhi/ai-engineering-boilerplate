// Claiming and releasing an execution's lease: the only place engine touches
// FOR UPDATE SKIP LOCKED. A single UPDATE...WHERE id = (SELECT ... FOR UPDATE SKIP LOCKED LIMIT
// 1)...RETURNING claims either a fresh Ready row or one whose lease expired (its owner crashed
// mid-tick) — one query serves both "new work" and "crash recovery", no separate sweep.

use chrono::Utc;
use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, DbErr, Statement};
use std::time::Duration;
use uuid::Uuid;

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
            WHERE status = 'ready' OR (status = 'running' AND lease_until < now())
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
