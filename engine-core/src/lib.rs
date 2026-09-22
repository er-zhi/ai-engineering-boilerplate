// Pure, deterministic graph-execution logic with no async, I/O, or database.

pub mod budget;
pub mod builder;
pub mod checkpoint;
pub mod event;
pub mod execution;
#[cfg(feature = "test-support")]
pub mod fakes;
pub mod graph;
pub mod ids;
pub mod llm_output;
pub mod ports;
#[cfg(test)]
mod replay;
pub mod state;
pub mod step;

pub use budget::Budget;
pub use builder::{GraphBuilder, agent_graph, rag_graph, simple_graph};
pub use checkpoint::{CHECKPOINT_SCHEMA_VERSION, Checkpoint};
pub use event::{Event, ExecutionEvent, ExecutionPayload};
pub use execution::{ActiveNode, Execution, Status};
pub use graph::{Condition, Edge, Graph, Node, Reducer, WaitKind, evaluate_condition};
pub use ids::{ExecutionId, GraphId, NodeId, UserId};
pub use llm_output::{
    FAST_TOOL_CALL_FIELD, LLM_STATE_KEY, LlmOutput, PRIOR_MATERIAL_STATE_KEY,
    RENDERED_TOOL_RESULTS, TOOL_RESULT_ERROR_KEY, TOOL_RESULT_STATE_KEY, ToolCall, ToolRecord,
    llm_reply_pointer, llm_tool_call_pointer,
};
pub use ports::{CheckpointStore, TaskError, TaskExecutor};
pub use state::apply_reducer;
pub use step::{NodeOutput, interrupt, step};
