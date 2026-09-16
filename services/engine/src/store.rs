// The two simplest ports: append a checkpoint, append an event. Both single-row inserts against
// entities Task 10 already defined, converted through Task 11's wire.rs.

use engine_core::{Checkpoint, CheckpointStore, EventSink, ExecutionEvent, ExecutionId};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder,
};

use crate::entity::checkpoint;
use crate::wire::{checkpoint_from_model, checkpoint_to_active_model, event_to_active_model};

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
            .map_err(|e| e.to_string())
    }

    async fn latest(&self, execution_id: ExecutionId) -> Result<Option<Checkpoint>, String> {
        let model = checkpoint::Entity::find()
            .filter(checkpoint::Column::ExecutionId.eq(execution_id.0))
            .order_by_desc(checkpoint::Column::Step)
            .one(&self.db)
            .await
            .map_err(|e| e.to_string())?;
        model.map(checkpoint_from_model).transpose()
    }
}

#[derive(Clone)]
pub struct PgEventSink {
    db: DatabaseConnection,
}

impl PgEventSink {
    #[must_use]
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }
}

impl EventSink for PgEventSink {
    async fn append(&self, event: &ExecutionEvent) -> Result<(), String> {
        event_to_active_model(event)
            .insert(&self.db)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

#[cfg(all(test, feature = "test-support"))]
mod tests {
    use super::*;
    use engine_core::{ActiveNode, Event, ExecutionPayload, NodeId};

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

    #[tokio::test(flavor = "multi_thread")]
    async fn event_sink_appends_without_error() {
        let test = crate::test_db::start().await;
        let sink = PgEventSink::new(test.db.clone());
        let event: ExecutionEvent = Event {
            id: uuid::Uuid::new_v4(),
            version: 1,
            occurred_at: chrono::Utc::now(),
            user_id: None,
            correlation_id: ExecutionId(uuid::Uuid::new_v4()),
            causation_id: None,
            payload: ExecutionPayload::ExecutionStarted,
        };
        sink.append(&event).await.expect("append");
    }
}
