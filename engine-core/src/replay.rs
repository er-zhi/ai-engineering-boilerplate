// Property: replaying an execution's ExecutionEvent log reconstructs the same status and state
// step() itself produced — proof that "everything is in the log" (the spec's premise for
// crash recovery and for Chat reading Engine's history) actually holds, not just an assertion.

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use serde_json::json;
    use std::time::Duration;

    use crate::budget::Budget;
    use crate::event::ExecutionPayload;
    use crate::execution::{ActiveNode, Execution, Status};
    use crate::graph::Condition;
    use crate::ids::{ExecutionId, GraphId, NodeId};
    use crate::step::{NodeOutput, step};

    /// Applies one event's effect to `execution` — the minimal projection needed for this
    /// property, not a general-purpose event sourcing API: only the payload variants `step()`
    /// actually emits for the fixed test graph below are handled.
    fn apply_event(execution: &mut Execution, event: &crate::event::ExecutionEvent) {
        match &event.payload {
            ExecutionPayload::ExecutionCompleted { final_state } => {
                execution.state = final_state.clone();
                execution.status = Status::Completed;
                execution.current_nodes = vec![];
            }
            ExecutionPayload::ExecutionFailed { .. } => {
                execution.status = Status::Failed;
            }
            _ => {}
        }
    }

    fn linear_graph() -> crate::graph::Graph {
        crate::builder::GraphBuilder::new()
            .entry("start")
            .task("start", "noop", json!({}))
            .end("end")
            .edge("start", "end", Condition::Always)
            .build("prop-test", 1)
    }

    proptest! {
        #[test]
        fn replaying_events_matches_the_execution_step_produced(seed in 0u64..1000) {
            let g = linear_graph();
            let exec = Execution {
                id: ExecutionId(uuid::Uuid::new_v4()),
                graph_id: GraphId("prop-test".into()),
                graph_version: 1,
                user_id: None,
                status: Status::Ready,
                current_nodes: vec![ActiveNode::plain(NodeId("start".into()))],
                state: json!({"seed": seed}),
                iteration: 0,
                max_iterations: 10,
                deadline: None,
                budget: Budget::new(1000, 10, Duration::from_secs(60)),
            };
            let outputs = vec![NodeOutput {
                node: ActiveNode::plain(NodeId("start".into())),
                result: Ok(json!({"seed": seed})),
            }];
            let (final_exec, events) = step(&g, exec.clone(), outputs, chrono::Utc::now());

            let mut replayed = exec;
            for event in &events {
                apply_event(&mut replayed, event);
            }

            prop_assert_eq!(replayed.status, final_exec.status);
            prop_assert_eq!(replayed.state.get("seed"), final_exec.state.get("seed"));
        }
    }
}
