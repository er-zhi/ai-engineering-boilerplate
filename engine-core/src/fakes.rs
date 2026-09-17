// Provides an in-memory TaskExecutor for tests.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::ports::{TaskError, TaskExecutor};

#[derive(Clone, Debug, PartialEq)]
pub struct RecordedCall {
    pub kind: String,
    pub config: serde_json::Value,
    pub idempotency_key: String,
}

#[derive(Default)]
pub struct FakeTaskExecutor {
    responses: Mutex<HashMap<String, Result<serde_json::Value, TaskError>>>,
    pub calls: Mutex<Vec<RecordedCall>>,
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
        self.calls.lock().expect("lock").push(RecordedCall {
            kind: kind.to_owned(),
            config: config.clone(),
            idempotency_key: idempotency_key.to_owned(),
        });
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
