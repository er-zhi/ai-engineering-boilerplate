// The one place Task.kind strings map to a real TaskExecutor implementation. Adding a new kind
// (Tool Service's future "tool") is a new match arm here — engine-core never changes.

use engine_core::{TaskError, TaskExecutor};
use serde_json::Value;

use crate::executors::llm::LlmTaskExecutor;

pub struct Dispatcher {
    pub llm: LlmTaskExecutor,
}

impl TaskExecutor for Dispatcher {
    async fn execute(
        &self,
        kind: &str,
        config: &Value,
        state: &Value,
        idempotency_key: &str,
    ) -> Result<Value, TaskError> {
        match kind {
            "llm" => self.llm.execute(kind, config, state, idempotency_key).await,
            other => Err(TaskError::Failed(format!(
                "no TaskExecutor wired for kind {other:?} yet — Tool Service adds \"tool\""
            ))),
        }
    }
}
