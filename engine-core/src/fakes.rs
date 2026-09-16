// In-memory fakes for the three ports, behind `test-support` — used by engine-core's own tests
// (none need them directly today; step() is tested without any port) and by the `engine`
// service's tests, which exercise the tick loop against these instead of real Postgres/LLM
// calls for anything that doesn't need testcontainers.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::checkpoint::Checkpoint;
use crate::event::ExecutionEvent;
use crate::ids::ExecutionId;
use crate::ports::{CheckpointStore, EventSink, TaskError, TaskExecutor};

#[derive(Default)]
pub struct FakeTaskExecutor {
    responses: Mutex<HashMap<String, Result<serde_json::Value, TaskError>>>,
    pub calls: Mutex<Vec<(String, serde_json::Value, String)>>, // (kind, config, idempotency_key)
}

impl FakeTaskExecutor {
    pub fn respond(&self, kind: &str, result: Result<serde_json::Value, TaskError>) {
        self.responses
            .lock()
            .expect("lock")
            .insert(kind.to_owned(), result);
    }
}

impl TaskExecutor for FakeTaskExecutor {
    async fn execute(
        &self,
        kind: &str,
        config: &serde_json::Value,
        _state: &serde_json::Value,
        idempotency_key: &str,
    ) -> Result<serde_json::Value, TaskError> {
        self.calls.lock().expect("lock").push((
            kind.to_owned(),
            config.clone(),
            idempotency_key.to_owned(),
        ));
        self.responses
            .lock()
            .expect("lock")
            .get(kind)
            .cloned()
            .unwrap_or_else(|| {
                Err(TaskError::Failed(format!(
                    "no fake response programmed for kind {kind}"
                )))
            })
    }
}

#[derive(Default)]
pub struct InMemoryCheckpointStore {
    by_execution: Mutex<HashMap<ExecutionId, Vec<Checkpoint>>>,
}

impl CheckpointStore for InMemoryCheckpointStore {
    async fn save(&self, checkpoint: &Checkpoint) -> Result<(), String> {
        self.by_execution
            .lock()
            .expect("lock")
            .entry(checkpoint.execution_id)
            .or_default()
            .push(checkpoint.clone());
        Ok(())
    }

    async fn latest(&self, execution_id: ExecutionId) -> Result<Option<Checkpoint>, String> {
        Ok(self
            .by_execution
            .lock()
            .expect("lock")
            .get(&execution_id)
            .and_then(|checkpoints| checkpoints.last().cloned()))
    }
}

#[derive(Default)]
pub struct InMemoryEventSink {
    pub events: Mutex<Vec<ExecutionEvent>>,
}

impl EventSink for InMemoryEventSink {
    async fn append(&self, event: &ExecutionEvent) -> Result<(), String> {
        self.events.lock().expect("lock").push(event.clone());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::NodeId;

    #[tokio::test]
    async fn fake_task_executor_returns_the_programmed_response() {
        let executor = FakeTaskExecutor::default();
        executor.respond("llm", Ok(serde_json::json!({"reply": "hi"})));

        let result = executor
            .execute(
                "llm",
                &serde_json::json!({}),
                &serde_json::json!({}),
                "exec1:llm:0",
            )
            .await;

        assert_eq!(result, Ok(serde_json::json!({"reply": "hi"})));
        assert_eq!(executor.calls.lock().expect("lock").len(), 1);
    }

    #[tokio::test]
    async fn checkpoint_store_returns_the_most_recently_saved_checkpoint() {
        let store = InMemoryCheckpointStore::default();
        let execution_id = ExecutionId(uuid::Uuid::new_v4());
        let checkpoint = |step| Checkpoint {
            schema_version: crate::checkpoint::CHECKPOINT_SCHEMA_VERSION,
            execution_id,
            step,
            state: serde_json::json!({}),
            current_nodes: vec![],
        };
        store.save(&checkpoint(0)).await.expect("save");
        store.save(&checkpoint(1)).await.expect("save");

        let latest = store
            .latest(execution_id)
            .await
            .expect("latest")
            .expect("some");
        assert_eq!(latest.step, 1);
    }

    #[tokio::test]
    async fn event_sink_records_every_appended_event() {
        let sink = InMemoryEventSink::default();
        let execution_id = ExecutionId(uuid::Uuid::new_v4());
        let event = crate::event::Event {
            id: uuid::Uuid::new_v4(),
            version: 1,
            occurred_at: chrono::Utc::now(),
            user_id: None,
            correlation_id: execution_id,
            causation_id: None,
            payload: crate::event::ExecutionPayload::NodeStarted {
                node: NodeId("llm".into()),
            },
        };
        sink.append(&event).await.expect("append");
        assert_eq!(sink.events.lock().expect("lock").len(), 1);
    }
}
