// The three seams between engine-core and everything that needs real I/O: running a Task
// (TaskExecutor), persisting a Checkpoint (CheckpointStore), and appending an ExecutionEvent
// (EventSink). No async_trait — impl Future<...> + Send in return position, matching
// common::cache::CacheStore's style. The engine service implements these against Postgres and
// llm-router/Tool Service; engine-core only ever sees the trait.

use crate::checkpoint::Checkpoint;
use crate::event::ExecutionEvent;
use crate::ids::ExecutionId;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TaskError {
    #[error("{0}")]
    Failed(String),
}

pub trait TaskExecutor: Send + Sync {
    fn execute(
        &self,
        kind: &str,
        config: &serde_json::Value,
        state: &serde_json::Value,
        idempotency_key: &str,
    ) -> impl Future<Output = Result<serde_json::Value, TaskError>> + Send;
}

pub trait CheckpointStore: Send + Sync {
    fn save(&self, checkpoint: &Checkpoint) -> impl Future<Output = Result<(), String>> + Send;
    fn latest(
        &self,
        execution_id: ExecutionId,
    ) -> impl Future<Output = Result<Option<Checkpoint>, String>> + Send;
}

pub trait EventSink: Send + Sync {
    fn append(&self, event: &ExecutionEvent) -> impl Future<Output = Result<(), String>> + Send;
}
