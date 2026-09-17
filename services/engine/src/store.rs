// Persists checkpoints and execution events.

use engine_core::{Checkpoint, CheckpointStore, Execution, ExecutionEvent, ExecutionId};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseConnection, DbErr, EntityTrait,
    QueryFilter, QueryOrder, TransactionTrait,
};

use crate::entity::checkpoint;
use crate::lease;
use crate::wire::{
    checkpoint_from_model, checkpoint_to_active_model, event_to_active_model, status_to_columns,
};

pub const WAKE_THE_NEXT_TICK: &str = "NOTIFY engine_tick";

const STORAGE_FAILURE: &str = "engine could not reach its own storage";

const LEASE_LOST: &str = "another worker holds this execution's lease";

fn storage_failure(operation: &str, error: &sea_orm::DbErr) -> String {
    tracing::error!(%error, operation, "engine storage call failed");
    STORAGE_FAILURE.to_owned()
}

#[derive(Clone)]
pub struct PgCheckpointStore {
    db: DatabaseConnection,
}

impl PgCheckpointStore {
    #[must_use]
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }
}

impl CheckpointStore for PgCheckpointStore {
    async fn save(&self, checkpoint: &Checkpoint) -> Result<(), String> {
        checkpoint_to_active_model(checkpoint)
            .insert(&self.db)
            .await
            .map(|_| ())
            .map_err(|e| storage_failure("save checkpoint", &e))
    }

    async fn latest(&self, execution_id: ExecutionId) -> Result<Option<Checkpoint>, String> {
        let model = checkpoint::Entity::find()
            .filter(checkpoint::Column::ExecutionId.eq(execution_id.0))
            .order_by_desc(checkpoint::Column::Step)
            .one(&self.db)
            .await
            .map_err(|e| storage_failure("read latest checkpoint", &e))?;
        model.map(checkpoint_from_model).transpose()
    }
}

pub async fn commit_step(
    db: &DatabaseConnection,
    execution_id: uuid::Uuid,
    owner: Option<&str>,
    step: u32,
    execution: &Execution,
    events: &[ExecutionEvent],
) -> Result<(), DbErr> {
    let checkpoint = Checkpoint {
        schema_version: engine_core::CHECKPOINT_SCHEMA_VERSION,
        execution_id: ExecutionId(execution_id),
        step,
        state: execution.state.clone(),
        current_nodes: execution.current_nodes.clone(),
    };
    let (status_column, wait_kind_column) = status_to_columns(&execution.status);
    let current_nodes_json = serde_json::to_value(&execution.current_nodes).unwrap_or_else(|_| {
        debug_assert!(false, "an ActiveNode list always serializes");
        serde_json::Value::Array(Vec::new())
    });

    let txn = db.begin().await?;
    checkpoint_to_active_model(&checkpoint).insert(&txn).await?;
    for event in events {
        event_to_active_model(event).insert(&txn).await?;
    }
    let released = lease::release_lease(
        &txn,
        execution_id,
        owner,
        status_column,
        wait_kind_column,
        current_nodes_json,
    )
    .await?;
    if released == 0 {
        txn.rollback().await?;
        return Err(DbErr::Custom(LEASE_LOST.to_owned()));
    }
    txn.execute_unprepared(WAKE_THE_NEXT_TICK).await?;
    txn.commit().await
}

#[cfg(all(test, feature = "test-support"))]
mod tests {
    use super::*;
    use engine_core::{ActiveNode, NodeId};

    #[tokio::test(flavor = "multi_thread")]
    async fn checkpoint_store_saves_and_reads_back_the_latest() {
        let test = crate::test_db::start().await;
        let store = PgCheckpointStore::new(test.db.clone());
        let execution_id = ExecutionId(uuid::Uuid::new_v4());
        let make = |step| Checkpoint {
            schema_version: engine_core::CHECKPOINT_SCHEMA_VERSION,
            execution_id,
            step,
            state: serde_json::json!({"step": step}),
            current_nodes: vec![ActiveNode::plain(NodeId("llm".into()))],
        };
        store.save(&make(0)).await.expect("save 0");
        store.save(&make(1)).await.expect("save 1");

        let latest = store
            .latest(execution_id)
            .await
            .expect("latest")
            .expect("some");
        assert_eq!(latest.step, 1);
    }
}
