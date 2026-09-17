// The initial state Chat starts an engine execution with: the one declaration its writer and its readers share.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct ExecutionInput {
    pub question: String,
}

impl ExecutionInput {
    #[must_use]
    pub fn new(question: impl Into<String>) -> Self {
        Self {
            question: question.into(),
        }
    }

    #[must_use]
    pub fn in_state(state: &Value) -> Self {
        serde_json::from_value(state.clone()).unwrap_or_default()
    }

    #[must_use]
    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or_else(|_| {
            debug_assert!(false, "ExecutionInput always serializes");
            Value::Object(serde_json::Map::new())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn what_chat_sends_is_what_the_executors_read_back() {
        let sent = ExecutionInput::new("What is Claude Code?");

        assert_eq!(ExecutionInput::in_state(&sent.to_json()), sent);
    }

    #[test]
    fn a_state_the_graph_has_since_added_to_still_reads_its_question() {
        let state = json!({"question": "q", "llm": {"reply": "42"}});

        assert_eq!(ExecutionInput::in_state(&state).question, "q");
    }

    #[test]
    fn a_state_with_no_question_reads_as_empty_rather_than_failing() {
        assert_eq!(ExecutionInput::in_state(&json!({})).question, "");
    }
}
