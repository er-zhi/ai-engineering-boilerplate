// The object an llm node writes into state — a tool call or a final reply — and the pointers into it.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const LLM_STATE_KEY: &str = "llm";
pub const TOOL_RESULT_STATE_KEY: &str = "tool_result";

pub const PRIOR_MATERIAL_STATE_KEY: &str = "prior_material";

pub const RENDERED_TOOL_RESULTS: usize = 4;
pub const TOOL_RESULT_ERROR_KEY: &str = "error";
pub const FAST_TOOL_CALL_FIELD: &str = "fast_tool_call";

const TOOL_CALL_FIELD: &str = "tool_call";
const REPLY_FIELD: &str = "reply";

#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct LlmOutput {
    pub tool_call: Value,
    pub reply: Value,
}

impl LlmOutput {
    #[must_use]
    pub fn in_state(state: &Value) -> Self {
        state
            .get(LLM_STATE_KEY)
            .and_then(|written| serde_json::from_value(written.clone()).ok())
            .unwrap_or_default()
    }

    #[must_use]
    pub fn is_blank(&self) -> bool {
        self.tool_call.is_null() && self.reply.is_null()
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct ToolCall {
    pub name: String,
    #[serde(default = "no_arguments")]
    pub args: Value,
}

fn no_arguments() -> Value {
    Value::Object(serde_json::Map::new())
}

#[must_use]
pub fn llm_tool_call_pointer() -> String {
    format!("/{LLM_STATE_KEY}/{TOOL_CALL_FIELD}")
}

#[must_use]
pub fn llm_reply_pointer() -> String {
    format!("/{LLM_STATE_KEY}/{REPLY_FIELD}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_pointers_resolve_inside_what_the_node_writes() {
        let state = json!({
            LLM_STATE_KEY: serde_json::to_value(LlmOutput {
                tool_call: json!({"name": "web_search", "args": {}}),
                reply: Value::Null,
            })
            .expect("serialize"),
        });

        assert_eq!(
            state.pointer(&llm_tool_call_pointer()),
            Some(&json!({"name": "web_search", "args": {}}))
        );
        assert_eq!(state.pointer(&llm_reply_pointer()), Some(&Value::Null));
    }

    #[test]
    fn a_state_with_no_llm_node_output_yet_reads_as_blank() {
        assert!(LlmOutput::in_state(&json!({})).is_blank());
        assert!(!LlmOutput::in_state(&json!({LLM_STATE_KEY: {"reply": "42"}})).is_blank());
    }

    #[test]
    fn a_call_without_arguments_still_carries_an_object() {
        let call: ToolCall = serde_json::from_value(json!({"name": "web_search"})).expect("parse");

        assert_eq!(call.args, json!({}));
    }
}
