// Keeping the hot tables hot. `executions` and `checkpoints` are the engine's working set — the
// queue and the restart point — not history: once an execution reaches a terminal status, its
// `execution_events` rows already carry everything anyone can ask about it (`ExecutionCompleted
// { final_state }` and friends are the final word, and `NodeCompleted { output }` replays the
// steps in `id` order). So after a grace period the row and its checkpoints go, and `StreamEvents`
// — not `GetExecution` — is the client's contract for history. See the spec's «Горячие и холодные
// данные, ретеншн».
//
// Called only from `wakeup.rs`'s fallback-interval arm, never on a `NOTIFY`: a busy engine must
// not sweep once per commit.

use std::time::Duration;

use chrono::Utc;
use sea_orm::{
    ColumnTrait, DatabaseConnection, DbErr, EntityTrait, QueryFilter, QueryOrder, QuerySelect,
    TransactionTrait,
};
use uuid::Uuid;

use crate::entity::{checkpoint, execution};

/// The status column values `wire::status_to_columns` writes for a finished execution.
const TERMINAL_STATUSES: [&str; 3] = ["completed", "failed", "cancelled"];

/// Deletes executions that reached a terminal status more than `retention` ago, together with
/// their checkpoints, and returns how many `executions` rows went. `execution_events` is
/// deliberately untouched — that is the analytic log this sweep is safe because of.
///
/// `batch` bounds one pass so the transaction stays short next to the claim query's
/// `FOR UPDATE SKIP LOCKED`; the next fallback tick takes the next batch.
pub async fn sweep_terminal(
    db: &DatabaseConnection,
    retention: Duration,
    batch: u32,
) -> Result<u64, DbErr> {
    let cutoff = Utc::now()
        - chrono::Duration::from_std(retention).unwrap_or_else(|_| chrono::Duration::zero());

    let txn = db.begin().await?;
    let ids: Vec<Uuid> = execution::Entity::find()
        .select_only()
        .column(execution::Column::Id)
        .filter(execution::Column::Status.is_in(TERMINAL_STATUSES))
        .filter(execution::Column::UpdatedAt.lt(cutoff))
        .order_by_asc(execution::Column::UpdatedAt)
        .limit(u64::from(batch))
        .into_tuple()
        .all(&txn)
        .await?;
    if ids.is_empty() {
        txn.rollback().await?;
        return Ok(0);
    }

    // Checkpoints first: an execution row without its checkpoints is a row the tick loop can no
    // longer restart, so if the transaction were ever split this is the half that must not be the
    // survivor. They go together here, but the order still says which way the dependency runs.
    checkpoint::Entity::delete_many()
        .filter(checkpoint::Column::ExecutionId.is_in(ids.clone()))
        .exec(&txn)
        .await?;
    let deleted = execution::Entity::delete_many()
        .filter(execution::Column::Id.is_in(ids))
        .exec(&txn)
        .await?;
    txn.commit().await?;
    Ok(deleted.rows_affected)
}
