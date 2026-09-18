// Advances an execution by one super-step: the one place graph semantics live.

use chrono::{DateTime, Utc};

use serde_json::json;

use crate::event::{ExecutionEvent, ExecutionPayload};
use crate::execution::{ActiveNode, Execution, Status};
use crate::graph::{Condition, Graph, Node, evaluate_condition};
use crate::state::apply_reducer;

const FAN_IN_TABLE: &str = "__fan_in";
const ONE_TOOL_CALL: u32 = 1;
const UNMEASURED_WALL_TIME: std::time::Duration = std::time::Duration::ZERO;

pub struct NodeOutput {
    pub node: ActiveNode,
    pub result: Result<serde_json::Value, String>,
}

pub fn step(
    graph: &Graph,
    mut execution: Execution,
    outputs: Vec<NodeOutput>,
    now: DateTime<Utc>,
) -> (Execution, Vec<ExecutionEvent>) {
    let mut events = Vec::new();
    let mut next: Vec<ActiveNode> = Vec::new();
    let mut hard_failure: Option<String> = None;

    if outputs.is_empty() {
        next.extend(execution.current_nodes.iter().cloned());
    }

    for output in &outputs {
        apply_output(graph, &mut execution, &mut events, output);
        if fan_handled_output(graph, &mut execution, output, &mut next) {
            continue;
        }
        record_edges(graph, &execution, output, &mut next, &mut hard_failure);
    }

    if let Some(error) = hard_failure {
        execution.status = Status::Failed;
        events.push(execution.event(ExecutionPayload::ExecutionFailed { error }));
        return (execution, events);
    }

    if !outputs.is_empty() && execution.budget.exhausted() {
        execution.status = Status::Failed;
        events.push(execution.event(ExecutionPayload::ExecutionFailed {
            error: "budget exhausted".to_owned(),
        }));
        return (execution, events);
    }
    if !outputs.is_empty() && execution.deadline.is_some_and(|deadline| now >= deadline) {
        execution.status = Status::Failed;
        events.push(execution.event(ExecutionPayload::ExecutionFailed {
            error: "deadline exceeded".to_owned(),
        }));
        return (execution, events);
    }

    next = expand_completed_fan_ins(graph, &execution, next);

    next.dedup_by(|a, b| a == b);
    execution.iteration += 1;

    if next
        .iter()
        .any(|active| matches!(graph.node(&active.node), Some(Node::End { .. })))
    {
        execution.current_nodes = Vec::new();
        execution.status = Status::Completed;
        events.push(execution.event(ExecutionPayload::ExecutionCompleted {
            result: graph.answer(&execution.state),
            final_state: execution.state.clone(),
        }));
        return (execution, events);
    }

    if execution.iteration >= execution.max_iterations {
        execution.status = Status::Failed;
        events.push(execution.event(ExecutionPayload::ExecutionFailed {
            error: format!("max_iterations ({}) reached", execution.max_iterations),
        }));
        return (execution, events);
    }

    park_next(graph, execution, events, next)
}

fn park_next(
    graph: &Graph,
    mut execution: Execution,
    mut events: Vec<ExecutionEvent>,
    next: Vec<ActiveNode>,
) -> (Execution, Vec<ExecutionEvent>) {
    let wait_on = next
        .iter()
        .find_map(|active| match graph.node(&active.node) {
            Some(Node::Wait { on, .. }) => Some(on.clone()),
            Some(Node::Subgraph { id, .. }) => {
                Some(crate::graph::WaitKind::ExternalEvent { key: id.0.clone() })
            }
            _ => None,
        });

    execution.current_nodes = next;
    match wait_on {
        Some(on) => {
            execution.status = Status::Waiting(on.clone());
            events.push(execution.event(ExecutionPayload::Waiting { on }));
        }
        None => execution.status = Status::Ready,
    }
    (execution, events)
}

pub fn interrupt(
    mut execution: Execution,
    input: serde_json::Value,
) -> (Execution, ExecutionEvent) {
    execution
        .state
        .as_object_mut()
        .expect("state is an object")
        .insert("interrupt_input".to_owned(), input);
    execution.status = Status::Ready;
    let event = execution.event(ExecutionPayload::Interrupted);
    (execution, event)
}

fn fan_handled_output(
    graph: &Graph,
    execution: &mut Execution,
    output: &NodeOutput,
    next: &mut Vec<ActiveNode>,
) -> bool {
    match graph.node(&output.node.node) {
        Some(Node::FanOut { source, target, .. }) => {
            let branch_count = execution
                .state
                .pointer(source)
                .and_then(serde_json::Value::as_array)
                .map_or(0, Vec::len);
            for index in 0..branch_count {
                next.push(ActiveNode {
                    node: target.clone(),
                    branch: Some(u32::try_from(index).unwrap_or(u32::MAX)),
                });
            }
            open_fan_in_barrier(graph, execution, target, branch_count);
            return true;
        }
        Some(Node::FanIn { .. }) => {
            return false;
        }
        _ => {}
    }

    if let Some(fan_in_node) = fan_in_consuming(graph, &output.node.node) {
        if output.result.is_err() {
            return false;
        }
        record_fan_in_branch(execution, output, fan_in_node, next);
        return true;
    }

    false
}

fn open_fan_in_barrier(
    graph: &Graph,
    execution: &mut Execution,
    branch_target: &crate::ids::NodeId,
    spawned_branches: usize,
) {
    let Some(Node::FanIn { id, .. }) = fan_in_consuming(graph, branch_target) else {
        return;
    };
    let fan_in_id = id.0.clone();
    execution
        .state
        .as_object_mut()
        .expect("state is an object")
        .entry(FAN_IN_TABLE)
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .expect("__fan_in is an object")
        .insert(
            fan_in_id,
            json!({"results": vec![serde_json::Value::Null; spawned_branches]}),
        );
}

fn record_fan_in_branch(
    execution: &mut Execution,
    output: &NodeOutput,
    fan_in_node: &Node,
    next: &mut Vec<ActiveNode>,
) {
    let Node::FanIn { id, key, reducer } = fan_in_node else {
        unreachable!()
    };
    let fan_in_id = id.clone();
    let key = key.clone();
    let reducer = *reducer;

    let branch_index = output
        .node
        .branch
        .expect("a FanOut target always carries a branch") as usize;
    let branch_result = output
        .result
        .clone()
        .expect("a failed branch never reaches the barrier");

    let fan_in_table = execution
        .state
        .as_object_mut()
        .expect("state is an object")
        .entry(FAN_IN_TABLE)
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .expect("__fan_in is an object");
    let slot = fan_in_table
        .entry(fan_in_id.0.clone())
        .or_insert_with(|| json!({"results": []}));
    let results = slot["results"].as_array_mut().expect("results is an array");
    if branch_index >= results.len() {
        results.resize(branch_index + 1, serde_json::Value::Null);
    }
    results[branch_index] = branch_result;

    let all_in = slot["results"]
        .as_array()
        .expect("results is an array")
        .iter()
        .all(|value| !value.is_null());
    if !all_in {
        return;
    }
    let folded: Vec<serde_json::Value> = slot["results"]
        .as_array()
        .expect("results is an array")
        .clone();
    for item in folded {
        apply_reducer(&mut execution.state, &key, reducer, item);
    }
    next.push(ActiveNode::plain(fan_in_id));
}

fn expand_completed_fan_ins(
    graph: &Graph,
    execution: &Execution,
    next: Vec<ActiveNode>,
) -> Vec<ActiveNode> {
    let mut expanded = Vec::new();
    for active in next {
        if matches!(graph.node(&active.node), Some(Node::FanIn { .. })) {
            for edge in graph.edges_from(&active.node) {
                if !matches!(edge.condition, Condition::Failed)
                    && evaluate_condition(&edge.condition, &execution.state)
                {
                    expanded.push(ActiveNode::plain(edge.to.clone()));
                }
            }
        } else {
            expanded.push(active);
        }
    }
    expanded
}

fn fan_in_consuming<'a>(graph: &'a Graph, node: &crate::ids::NodeId) -> Option<&'a Node> {
    graph
        .edges_from(node)
        .find_map(|edge| graph.node(&edge.to))
        .filter(|candidate| matches!(candidate, Node::FanIn { .. }))
}

fn apply_output(
    graph: &Graph,
    execution: &mut Execution,
    events: &mut Vec<ExecutionEvent>,
    output: &NodeOutput,
) {
    match &output.result {
        Ok(value) => {
            events.push(execution.event(ExecutionPayload::NodeCompleted {
                node: output.node.node.clone(),
                output: value.clone(),
            }));
            if let Some(Node::Task { config, .. }) = graph.node(&output.node.node) {
                charge_budget(execution, value);
                if let (Some(key), Some(reducer)) = (state_key(config), reducer_of(config)) {
                    apply_reducer(&mut execution.state, key, reducer, value.clone());
                }
            }
        }
        Err(error) => {
            events.push(execution.event(ExecutionPayload::NodeFailed {
                node: output.node.node.clone(),
                error: error.clone(),
            }));
        }
    }
}

fn record_edges(
    graph: &Graph,
    execution: &Execution,
    output: &NodeOutput,
    next: &mut Vec<ActiveNode>,
    hard_failure: &mut Option<String>,
) {
    let failed = output.result.is_err();
    let mut any_edge_fired = false;
    for edge in graph.edges_from(&output.node.node) {
        let fires = if failed {
            matches!(edge.condition, Condition::Failed)
        } else {
            !matches!(edge.condition, Condition::Failed)
                && evaluate_condition(&edge.condition, &execution.state)
        };
        if fires {
            any_edge_fired = true;
            next.push(ActiveNode::plain(edge.to.clone()));
        }
    }
    if failed && !any_edge_fired {
        *hard_failure = Some(format!(
            "node {} failed with no Failed edge: {}",
            output.node.node,
            output.result.as_ref().expect_err("checked above")
        ));
    }
}

/// The old `llm` → `tool` → `llm` loop charges `ONE_TOOL_CALL` for the deciding `llm` node's own
/// output and a second `ONE_TOOL_CALL` for the `tool` node's — two charges for one tool used. An
/// `llm` node whose output carries `FAST_TOOL_CALL_FIELD` (see that constant's doc) dispatched a
/// tool itself, outside the graph, so it never earns the `tool` node's own charge on its own —
/// charge it here instead, so a fast-dispatched tool costs `tool_calls_remaining` exactly what the
/// node-per-node loop would have charged it, not less.
fn extra_tool_call_charge(value: &serde_json::Value) -> u32 {
    u32::from(value.get(crate::llm_output::FAST_TOOL_CALL_FIELD).is_some())
}

fn charge_budget(execution: &mut Execution, value: &serde_json::Value) {
    let tokens = value.get("usage").map_or(0, |usage| {
        let field = |name: &str| {
            usage
                .get(name)
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0)
        };
        u32::try_from(field("tokens_in").saturating_add(field("tokens_out"))).unwrap_or(u32::MAX)
    });
    let tool_calls = ONE_TOOL_CALL.saturating_add(extra_tool_call_charge(value));
    execution
        .budget
        .charge(tokens, tool_calls, UNMEASURED_WALL_TIME);
}

fn state_key(config: &serde_json::Value) -> Option<&str> {
    config.get("state_key").and_then(serde_json::Value::as_str)
}

fn reducer_of(config: &serde_json::Value) -> Option<crate::graph::Reducer> {
    match config.get("reducer") {
        None => Some(crate::graph::Reducer::Replace),
        Some(declared) => serde_json::from_value(declared.clone()).ok(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::Budget;
    use crate::graph::{Edge, Reducer, WaitKind};
    use crate::ids::{ExecutionId, GraphId, NodeId};
    use serde_json::json;
    use std::time::Duration;

    fn graph(nodes: Vec<Node>, edges: Vec<Edge>, entry: &str) -> Graph {
        Graph {
            id: GraphId("test".to_owned()),
            version: 1,
            user_id: None,
            nodes,
            edges,
            entry: NodeId(entry.to_owned()),
            answer_pointer: None,
        }
    }

    fn execution(entry: &str) -> Execution {
        Execution {
            id: ExecutionId(uuid::Uuid::new_v4()),
            graph_id: GraphId("test".to_owned()),
            graph_version: 1,
            user_id: None,
            status: Status::Ready,
            current_nodes: vec![ActiveNode::plain(NodeId(entry.to_owned()))],
            state: json!({}),
            iteration: 0,
            max_iterations: 10,
            deadline: None,
            budget: Budget::new(10_000, 100, Duration::from_secs(3600)),
        }
    }

    fn ok(node: &str, value: serde_json::Value) -> NodeOutput {
        NodeOutput {
            node: ActiveNode::plain(NodeId(node.to_owned())),
            result: Ok(value),
        }
    }

    #[test]
    fn a_task_flows_into_end_and_completes() {
        let g = graph(
            vec![
                Node::Task {
                    id: NodeId("a".into()),
                    kind: "noop".into(),
                    config: json!({}),
                },
                Node::End {
                    id: NodeId("end".into()),
                },
            ],
            vec![Edge {
                from: NodeId("a".into()),
                to: NodeId("end".into()),
                condition: Condition::Always,
            }],
            "a",
        );
        let exec = execution("a");
        let (exec, events) = step(&g, exec, vec![ok("a", json!({"reply": "hi"}))], Utc::now());

        assert_eq!(exec.status, Status::Completed);
        assert!(exec.current_nodes.is_empty());
        assert!(
            events
                .iter()
                .any(|e| matches!(e.payload, ExecutionPayload::ExecutionCompleted { .. }))
        );
    }

    fn answering_graph(answer_pointer: Option<&str>) -> Graph {
        let mut g = graph(
            vec![
                Node::Task {
                    id: NodeId("llm".into()),
                    kind: "llm".into(),
                    config: json!({"state_key": "llm", "reducer": "Replace"}),
                },
                Node::End {
                    id: NodeId("end".into()),
                },
            ],
            vec![Edge {
                from: NodeId("llm".into()),
                to: NodeId("end".into()),
                condition: Condition::Always,
            }],
            "llm",
        );
        g.answer_pointer = answer_pointer.map(str::to_owned);
        g
    }

    fn completion_result(events: &[ExecutionEvent]) -> Option<serde_json::Value> {
        events.iter().find_map(|event| match &event.payload {
            ExecutionPayload::ExecutionCompleted { result, .. } => result.clone(),
            _ => None,
        })
    }

    #[test]
    fn the_completed_event_carries_the_answer_the_graph_declares() {
        let g = answering_graph(Some("/llm/reply"));
        let (_, events) = step(
            &g,
            execution("llm"),
            vec![ok("llm", json!({"reply": "62F and foggy"}))],
            Utc::now(),
        );

        assert_eq!(completion_result(&events), Some(json!("62F and foggy")));
    }

    #[test]
    fn a_graph_declaring_no_answer_completes_with_no_result_and_no_error() {
        let g = answering_graph(None);
        let (exec, events) = step(
            &g,
            execution("llm"),
            vec![ok("llm", json!({"reply": "62F and foggy"}))],
            Utc::now(),
        );

        assert_eq!(exec.status, Status::Completed);
        assert_eq!(completion_result(&events), None);
    }

    #[test]
    fn the_completed_event_still_carries_the_whole_final_state() {
        let g = answering_graph(Some("/llm/reply"));
        let (_, events) = step(
            &g,
            execution("llm"),
            vec![ok("llm", json!({"reply": "62F and foggy"}))],
            Utc::now(),
        );

        let final_state = events.iter().find_map(|event| match &event.payload {
            ExecutionPayload::ExecutionCompleted { final_state, .. } => Some(final_state.clone()),
            _ => None,
        });
        assert_eq!(
            final_state,
            Some(json!({"llm": {"reply": "62F and foggy"}}))
        );
    }

    #[test]
    fn all_edges_whose_condition_is_true_fire() {
        let g = graph(
            vec![
                Node::Task {
                    id: NodeId("check".into()),
                    kind: "noop".into(),
                    config: json!({}),
                },
                Node::Task {
                    id: NodeId("yes".into()),
                    kind: "noop".into(),
                    config: json!({}),
                },
                Node::Task {
                    id: NodeId("also".into()),
                    kind: "noop".into(),
                    config: json!({}),
                },
                Node::End {
                    id: NodeId("end".into()),
                },
            ],
            vec![
                Edge {
                    from: NodeId("check".into()),
                    to: NodeId("yes".into()),
                    condition: Condition::Always,
                },
                Edge {
                    from: NodeId("check".into()),
                    to: NodeId("also".into()),
                    condition: Condition::Always,
                },
                Edge {
                    from: NodeId("yes".into()),
                    to: NodeId("end".into()),
                    condition: Condition::Always,
                },
                Edge {
                    from: NodeId("also".into()),
                    to: NodeId("end".into()),
                    condition: Condition::Always,
                },
            ],
            "check",
        );
        let exec = execution("check");
        let (exec, _) = step(&g, exec, vec![ok("check", json!(null))], Utc::now());

        let mut names: Vec<_> = exec
            .current_nodes
            .iter()
            .map(|n| n.node.0.clone())
            .collect();
        names.sort();
        assert_eq!(names, vec!["also".to_owned(), "yes".to_owned()]);
    }

    #[test]
    fn a_node_output_is_merged_into_state_before_conditions_are_evaluated() {
        let g = graph(
            vec![
                Node::Task {
                    id: NodeId("llm".into()),
                    kind: "noop".into(),
                    config: json!({"state_key": "tool_call", "reducer": "Replace"}),
                },
                Node::Task {
                    id: NodeId("tool".into()),
                    kind: "noop".into(),
                    config: json!({}),
                },
                Node::End {
                    id: NodeId("end".into()),
                },
            ],
            vec![
                Edge {
                    from: NodeId("llm".into()),
                    to: NodeId("tool".into()),
                    condition: Condition::Truthy("/tool_call".to_owned()),
                },
                Edge {
                    from: NodeId("llm".into()),
                    to: NodeId("end".into()),
                    condition: Condition::Not(Box::new(Condition::Truthy("/tool_call".to_owned()))),
                },
            ],
            "llm",
        );
        let exec = execution("llm");
        let (exec, _) = step(
            &g,
            exec,
            vec![ok("llm", json!({"name": "web_search"}))],
            Utc::now(),
        );

        assert_eq!(exec.state, json!({"tool_call": {"name": "web_search"}}));
        assert_eq!(
            exec.current_nodes,
            vec![ActiveNode::plain(NodeId("tool".into()))]
        );
    }

    #[test]
    fn a_failed_node_without_a_failed_edge_fails_the_whole_execution() {
        let g = graph(
            vec![
                Node::Task {
                    id: NodeId("a".into()),
                    kind: "noop".into(),
                    config: json!({}),
                },
                Node::End {
                    id: NodeId("end".into()),
                },
            ],
            vec![Edge {
                from: NodeId("a".into()),
                to: NodeId("end".into()),
                condition: Condition::Always,
            }],
            "a",
        );
        let exec = execution("a");
        let output = NodeOutput {
            node: ActiveNode::plain(NodeId("a".into())),
            result: Err("boom".into()),
        };
        let (exec, events) = step(&g, exec, vec![output], Utc::now());

        assert_eq!(exec.status, Status::Failed);
        assert!(events.iter().any(|e| matches!(&e.payload, ExecutionPayload::ExecutionFailed { error } if error.contains("boom"))));
    }

    #[test]
    fn a_failed_edge_recovers_from_a_failed_node() {
        let g = graph(
            vec![
                Node::Task {
                    id: NodeId("a".into()),
                    kind: "noop".into(),
                    config: json!({}),
                },
                Node::Task {
                    id: NodeId("recover".into()),
                    kind: "noop".into(),
                    config: json!({}),
                },
            ],
            vec![Edge {
                from: NodeId("a".into()),
                to: NodeId("recover".into()),
                condition: Condition::Failed,
            }],
            "a",
        );
        let exec = execution("a");
        let output = NodeOutput {
            node: ActiveNode::plain(NodeId("a".into())),
            result: Err("boom".into()),
        };
        let (exec, _) = step(&g, exec, vec![output], Utc::now());

        assert_eq!(exec.status, Status::Ready);
        assert_eq!(
            exec.current_nodes,
            vec![ActiveNode::plain(NodeId("recover".into()))]
        );
    }

    #[test]
    fn a_cycle_without_end_hits_max_iterations() {
        let g = graph(
            vec![Node::Task {
                id: NodeId("loop".into()),
                kind: "noop".into(),
                config: json!({}),
            }],
            vec![Edge {
                from: NodeId("loop".into()),
                to: NodeId("loop".into()),
                condition: Condition::Always,
            }],
            "loop",
        );
        let mut exec = execution("loop");
        exec.max_iterations = 3;
        for i in 0..3 {
            let (next, _) = step(&g, exec, vec![ok("loop", json!(null))], Utc::now());
            exec = next;
            if i < 2 {
                assert_eq!(exec.status, Status::Ready, "iteration {i}");
            }
        }
        assert_eq!(exec.status, Status::Failed);
    }

    #[test]
    fn fan_out_spawns_one_branch_per_array_element() {
        let g = graph(
            vec![
                Node::FanOut {
                    id: NodeId("spread".into()),
                    source: "/items".into(),
                    item_var: "item".into(),
                    target: NodeId("work".into()),
                },
                Node::Task {
                    id: NodeId("work".into()),
                    kind: "noop".into(),
                    config: json!({}),
                },
                Node::FanIn {
                    id: NodeId("join".into()),
                    key: "results".into(),
                    reducer: Reducer::Append,
                },
            ],
            vec![Edge {
                from: NodeId("work".into()),
                to: NodeId("join".into()),
                condition: Condition::Always,
            }],
            "spread",
        );
        let mut exec = execution("spread");
        exec.state = json!({"items": ["a", "b", "c"]});
        let (exec, _) = step(
            &g,
            exec,
            vec![NodeOutput {
                node: ActiveNode::plain(NodeId("spread".into())),
                result: Ok(json!(null)),
            }],
            Utc::now(),
        );

        let mut branches: Vec<_> = exec
            .current_nodes
            .iter()
            .map(|n| (n.node.0.clone(), n.branch))
            .collect();
        branches.sort_by_key(|(_, branch)| *branch);
        assert_eq!(
            branches,
            vec![
                ("work".to_owned(), Some(0)),
                ("work".to_owned(), Some(1)),
                ("work".to_owned(), Some(2)),
            ]
        );
    }

    fn fan_out_join_graph() -> Graph {
        graph(
            vec![
                Node::FanOut {
                    id: NodeId("spread".into()),
                    source: "/items".into(),
                    item_var: "item".into(),
                    target: NodeId("work".into()),
                },
                Node::Task {
                    id: NodeId("work".into()),
                    kind: "noop".into(),
                    config: json!({}),
                },
                Node::FanIn {
                    id: NodeId("join".into()),
                    key: "results".into(),
                    reducer: Reducer::Append,
                },
                Node::End {
                    id: NodeId("end".into()),
                },
            ],
            vec![
                Edge {
                    from: NodeId("work".into()),
                    to: NodeId("join".into()),
                    condition: Condition::Always,
                },
                Edge {
                    from: NodeId("join".into()),
                    to: NodeId("end".into()),
                    condition: Condition::Always,
                },
            ],
            "spread",
        )
    }

    #[test]
    fn fan_in_waits_for_every_branch_then_folds_results() {
        let g = fan_out_join_graph();
        let mut exec = execution("spread");
        exec.state = json!({"items": ["a", "b"]});
        let (exec, _) = step(
            &g,
            exec,
            vec![NodeOutput {
                node: ActiveNode::plain(NodeId("spread".into())),
                result: Ok(json!(null)),
            }],
            Utc::now(),
        );
        assert_eq!(exec.current_nodes.len(), 2, "both branches spawned");

        let branch0 = exec
            .current_nodes
            .iter()
            .find(|n| n.branch == Some(0))
            .cloned()
            .expect("branch 0");
        let (exec, _) = step(
            &g,
            exec,
            vec![NodeOutput {
                node: branch0,
                result: Ok(json!("A")),
            }],
            Utc::now(),
        );
        assert!(
            exec.current_nodes.is_empty(),
            "waiting on the second branch, nothing active"
        );

        let branch1 = ActiveNode {
            node: NodeId("work".into()),
            branch: Some(1),
        };
        let (exec, _) = step(
            &g,
            exec,
            vec![NodeOutput {
                node: branch1,
                result: Ok(json!("B")),
            }],
            Utc::now(),
        );
        assert_eq!(exec.status, Status::Completed);
        assert_eq!(exec.state.get("results"), Some(&json!(["A", "B"])));
    }

    #[test]
    fn a_failed_fan_out_branch_without_a_failed_edge_fails_the_whole_execution() {
        let g = fan_out_join_graph();
        let mut exec = execution("spread");
        exec.state = json!({"items": ["a", "b"]});
        let (exec, _) = step(
            &g,
            exec,
            vec![NodeOutput {
                node: ActiveNode::plain(NodeId("spread".into())),
                result: Ok(json!(null)),
            }],
            Utc::now(),
        );
        assert_eq!(exec.current_nodes.len(), 2, "both branches spawned");

        let (exec, events) = step(
            &g,
            exec,
            vec![NodeOutput {
                node: ActiveNode {
                    node: NodeId("work".into()),
                    branch: Some(0),
                },
                result: Err("branch blew up".into()),
            }],
            Utc::now(),
        );

        assert_eq!(exec.status, Status::Failed);
        assert!(events.iter().any(|e| matches!(
            &e.payload,
            ExecutionPayload::ExecutionFailed { error } if error.contains("branch blew up")
        )));
    }

    #[test]
    fn a_fan_in_barrier_keeps_its_spawn_time_size_when_the_source_array_is_rewritten() {
        let mut g = fan_out_join_graph();
        g.nodes.push(Node::Task {
            id: NodeId("other".into()),
            kind: "noop".into(),
            config: json!({"state_key": "items", "reducer": "Replace"}),
        });
        let mut exec = execution("spread");
        exec.state = json!({"items": ["a", "b"]});
        let (exec, _) = step(
            &g,
            exec,
            vec![NodeOutput {
                node: ActiveNode::plain(NodeId("spread".into())),
                result: Ok(json!(null)),
            }],
            Utc::now(),
        );
        assert_eq!(exec.current_nodes.len(), 2, "both branches spawned");

        let (exec, _) = step(
            &g,
            exec,
            vec![
                ok("other", json!([])),
                NodeOutput {
                    node: ActiveNode {
                        node: NodeId("work".into()),
                        branch: Some(0),
                    },
                    result: Ok(json!("A")),
                },
                NodeOutput {
                    node: ActiveNode {
                        node: NodeId("work".into()),
                        branch: Some(1),
                    },
                    result: Ok(json!("B")),
                },
            ],
            Utc::now(),
        );

        assert_eq!(exec.status, Status::Completed);
        assert_eq!(exec.state.get("items"), Some(&json!([])));
        assert_eq!(exec.state.get("results"), Some(&json!(["A", "B"])));
    }

    #[test]
    fn a_task_output_charges_its_reported_token_usage_and_one_tool_call() {
        let g = graph(
            vec![
                Node::Task {
                    id: NodeId("a".into()),
                    kind: "llm".into(),
                    config: json!({}),
                },
                Node::End {
                    id: NodeId("end".into()),
                },
            ],
            vec![Edge {
                from: NodeId("a".into()),
                to: NodeId("end".into()),
                condition: Condition::Always,
            }],
            "a",
        );
        let exec = execution("a");
        let (tokens_before, calls_before) = (
            exec.budget.tokens_remaining,
            exec.budget.tool_calls_remaining,
        );

        let (exec, _) = step(
            &g,
            exec,
            vec![ok(
                "a",
                json!({"reply": "hi", "usage": {"tokens_in": 100, "tokens_out": 50}}),
            )],
            Utc::now(),
        );

        assert_eq!(exec.budget.tokens_remaining, tokens_before - 150);
        assert_eq!(exec.budget.tool_calls_remaining, calls_before - 1);
    }

    #[test]
    fn a_task_output_without_usage_charges_only_the_tool_call() {
        let g = graph(
            vec![Node::Task {
                id: NodeId("loop".into()),
                kind: "tool".into(),
                config: json!({}),
            }],
            vec![Edge {
                from: NodeId("loop".into()),
                to: NodeId("loop".into()),
                condition: Condition::Always,
            }],
            "loop",
        );
        let exec = execution("loop");
        let (tokens_before, calls_before) = (
            exec.budget.tokens_remaining,
            exec.budget.tool_calls_remaining,
        );

        let (exec, _) = step(&g, exec, vec![ok("loop", json!({"ok": true}))], Utc::now());

        assert_eq!(exec.budget.tokens_remaining, tokens_before);
        assert_eq!(exec.budget.tool_calls_remaining, calls_before - 1);
    }

    // An `llm` node output carrying `FAST_TOOL_CALL_FIELD` stood in for a whole `llm` + `tool`
    // pair of node completions (see `extra_tool_call_charge`'s doc), so it must charge
    // `tool_calls_remaining` by two, not the flat one every other task output charges.
    #[test]
    fn a_task_output_carrying_a_fast_dispatched_tool_call_charges_two_tool_calls() {
        let g = graph(
            vec![
                Node::Task {
                    id: NodeId("a".into()),
                    kind: "llm".into(),
                    config: json!({}),
                },
                Node::End {
                    id: NodeId("end".into()),
                },
            ],
            vec![Edge {
                from: NodeId("a".into()),
                to: NodeId("end".into()),
                condition: Condition::Always,
            }],
            "a",
        );
        let exec = execution("a");
        let calls_before = exec.budget.tool_calls_remaining;

        let (exec, _) = step(
            &g,
            exec,
            vec![ok(
                "a",
                json!({
                    "reply": "68",
                    crate::llm_output::FAST_TOOL_CALL_FIELD: {"name": "web_search", "args": {}, "result": {}},
                }),
            )],
            Utc::now(),
        );

        assert_eq!(exec.budget.tool_calls_remaining, calls_before - 2);
    }

    #[test]
    fn a_wait_node_parks_the_execution() {
        let g = graph(
            vec![
                Node::Task {
                    id: NodeId("ask".into()),
                    kind: "noop".into(),
                    config: json!({}),
                },
                Node::Wait {
                    id: NodeId("pause".into()),
                    on: WaitKind::UserInput,
                },
            ],
            vec![Edge {
                from: NodeId("ask".into()),
                to: NodeId("pause".into()),
                condition: Condition::Always,
            }],
            "ask",
        );
        let exec = execution("ask");
        let (exec, events) = step(&g, exec, vec![ok("ask", json!(null))], Utc::now());

        assert_eq!(exec.status, Status::Waiting(WaitKind::UserInput));
        assert_eq!(
            exec.current_nodes,
            vec![ActiveNode::plain(NodeId("pause".into()))]
        );
        assert!(events.iter().any(|e| matches!(
            &e.payload,
            ExecutionPayload::Waiting {
                on: WaitKind::UserInput
            }
        )));
    }

    #[test]
    fn interrupt_wakes_a_waiting_execution_and_records_input() {
        let mut exec = execution("pause");
        exec.status = Status::Waiting(WaitKind::UserInput);
        exec.state = json!({"messages": ["first"]});

        let (exec, event) = interrupt(exec, json!({"role": "user", "text": "second"}));

        assert_eq!(exec.status, Status::Ready);
        assert!(matches!(event.payload, ExecutionPayload::Interrupted));
        assert_eq!(
            exec.state.get("interrupt_input"),
            Some(&json!({"role": "user", "text": "second"}))
        );
    }

    #[test]
    fn subgraph_starts_a_child_and_waits_for_it() {
        let g = graph(
            vec![Node::Subgraph {
                id: NodeId("agent_call".into()),
                graph_id: GraphId("agent".into()),
                version: None,
                input: "/question".into(),
                output_key: "answer".into(),
            }],
            vec![],
            "agent_call",
        );
        let mut exec = execution("agent_call");
        exec.state = json!({"question": "weather in SF?"});
        let (exec, events) = step(&g, exec, vec![], Utc::now());

        assert_eq!(
            exec.status,
            Status::Waiting(WaitKind::ExternalEvent {
                key: "agent_call".to_owned()
            })
        );
        assert!(events.iter().any(|e| matches!(
            &e.payload,
            ExecutionPayload::Waiting { on: WaitKind::ExternalEvent { key } } if key == "agent_call"
        )));
    }

    #[test]
    fn exhausted_budget_fails_the_execution() {
        let g = graph(
            vec![
                Node::Task {
                    id: NodeId("a".into()),
                    kind: "noop".into(),
                    config: json!({}),
                },
                Node::Task {
                    id: NodeId("b".into()),
                    kind: "noop".into(),
                    config: json!({}),
                },
            ],
            vec![Edge {
                from: NodeId("a".into()),
                to: NodeId("b".into()),
                condition: Condition::Always,
            }],
            "a",
        );
        let mut exec = execution("a");
        exec.budget = Budget::new(0, 10, Duration::from_secs(60));
        let (exec, events) = step(&g, exec, vec![ok("a", json!(null))], Utc::now());

        assert_eq!(exec.status, Status::Failed);
        assert!(events.iter().any(|e| matches!(&e.payload, ExecutionPayload::ExecutionFailed { error } if error.contains("budget"))));
    }

    #[test]
    fn a_past_deadline_fails_the_execution() {
        let g = graph(
            vec![
                Node::Task {
                    id: NodeId("a".into()),
                    kind: "noop".into(),
                    config: json!({}),
                },
                Node::Task {
                    id: NodeId("b".into()),
                    kind: "noop".into(),
                    config: json!({}),
                },
            ],
            vec![Edge {
                from: NodeId("a".into()),
                to: NodeId("b".into()),
                condition: Condition::Always,
            }],
            "a",
        );
        let mut exec = execution("a");
        exec.deadline = Some(Utc::now() - chrono::Duration::seconds(1));
        let (exec, events) = step(&g, exec, vec![ok("a", json!(null))], Utc::now());

        assert_eq!(exec.status, Status::Failed);
        assert!(events.iter().any(|e| matches!(&e.payload, ExecutionPayload::ExecutionFailed { error } if error.contains("deadline"))));
    }
}
