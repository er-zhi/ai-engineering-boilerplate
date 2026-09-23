// Declares the one I/O seam between engine-core and its runtime: persistence is not one of
// them, because `step` is handed data and hands data back, and the service keeps the database.

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
