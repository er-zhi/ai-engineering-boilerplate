// The object an llm node writes into state — a tool call or a final reply — and the pointers into it.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const LLM_STATE_KEY: &str = "llm";
pub const TOOL_RESULT_STATE_KEY: &str = "tool_result";

/// What an earlier turn of the same conversation found, carried into this one. Deliberately not
/// `TOOL_RESULT_STATE_KEY`: the gate that decides whether a turn has fetched anything reads that
/// key, and material inherited from before is not something *this* turn fetched. Sharing the key
/// would make every follow-up skip the decision that settles whether it may be answered at all.
pub const PRIOR_MATERIAL_STATE_KEY: &str = "prior_material";
pub const TOOL_RESULT_ERROR_KEY: &str = "error";
/// Set on an `llm` node's own output when its executor dispatched a tool itself, outside the
/// graph's normal `llm` → `tool` → `llm` loop — see `services/engine/src/executors/llm.rs`'s
/// typed-decision fast path. The node's output is recorded verbatim in its `NodeCompleted` event
/// and written into the checkpointed state by the node's own reducer either way, so naming this
/// field once here, rather than inventing it ad hoc in the executor, is what lets `step::charge_budget`
/// (in the same crate) charge for the call the same way it would have charged the `tool` node this
/// call stood in for.
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
