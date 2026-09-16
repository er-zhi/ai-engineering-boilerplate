// engine-core: the pure graph-execution logic — Graph, Node, Edge, State, Execution,
// Checkpoint, Event, and the one function (step) that ties them together. No async, no I/O, no
// database: everything here is deterministic and unit-testable without a running Postgres or a
// running engine service. See docs/superpowers/specs/2026-09-15-engine-design.md.

pub mod budget;
pub mod builder;
pub mod checkpoint;
pub mod event;
pub mod execution;
#[cfg(feature = "test-support")]
pub mod fakes;
pub mod graph;
pub mod ids;
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
pub use ports::{CheckpointStore, EventSink, TaskError, TaskExecutor};
pub use state::apply_reducer;
pub use step::{NodeOutput, interrupt, step};
