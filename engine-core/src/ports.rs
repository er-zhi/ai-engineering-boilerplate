// Declares the two I/O seams between engine-core and its runtime.

use crate::checkpoint::Checkpoint;
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
