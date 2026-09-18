// Deletes terminal executions and their checkpoints past the retention.

use std::time::Duration;

use chrono::Utc;
use sea_orm::{
    ColumnTrait, DatabaseConnection, DbErr, EntityTrait, QueryFilter, QueryOrder, QuerySelect,
    TransactionTrait,
};
use uuid::Uuid;

use crate::entity::execution::TERMINAL_STATUSES;
use crate::entity::{checkpoint, execution, pending_input};

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

    checkpoint::Entity::delete_many()
        .filter(checkpoint::Column::ExecutionId.is_in(ids.clone()))
        .exec(&txn)
        .await?;
    // `commit_step` already deletes a row the moment it is drained (applied or discarded), so
    // this normally finds nothing. It exists for the one case that path cannot close: input
    // queued in the narrow window between the commit that finishes an execution and the moment
    // that commit is visible. Without this, such a row would have no execution left to join
    // against once the row below deletes it, and would sit orphaned forever.
    pending_input::Entity::delete_many()
        .filter(pending_input::Column::ExecutionId.is_in(ids.clone()))
        .exec(&txn)
        .await?;
    let deleted = execution::Entity::delete_many()
        .filter(execution::Column::Id.is_in(ids))
        .exec(&txn)
        .await?;
    txn.commit().await?;
    Ok(deleted.rows_affected)
}
