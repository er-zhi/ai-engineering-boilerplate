// step(): the one place graph semantics live. Pure and synchronous — given what each active
// node produced this super-step, decide what state changed, which edges fire, and what the
// execution's next position and status are. See the spec's "step()" section.

use chrono::{DateTime, Utc};

use serde_json::json;

use crate::event::{ExecutionEvent, ExecutionPayload};
use crate::execution::{ActiveNode, Execution, Status};
use crate::graph::{Condition, Graph, Node, evaluate_condition};
use crate::state::apply_reducer;

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
        if handle_fan(graph, &mut execution, output, &mut next) {
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

    // A `FanIn` node completed its barrier this tick and was pushed into `next` directly (it
    // never goes through a `TaskExecutor`, so it can't wait for a future output the way a `Task`
    // does) — expand it into its own outgoing edges' targets immediately, in the same tick,
    // rather than parking it as an active node waiting to be "run".
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

/// Decides the execution's final status/position for this tick from `next`: `Status::Waiting`
/// on a `Wait` or not-yet-started `Subgraph` node found in it (a `Subgraph` waits on its own id
/// as an `ExternalEvent` key, so a `StreamEvents` watcher sees "blocked on something" uniformly
/// either way), or plain `Status::Ready` otherwise. Either way `next` becomes `current_nodes`.
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

/// The pure half of resuming a `Wait::UserInput`-parked execution: records the new input under
/// a reserved state key and flips status back to `Ready`. Writing this to Postgres and sending
/// `NOTIFY` is the service's job (Task 12) — this function only computes what changes.
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

/// Handles `FanOut`/`FanIn` semantics for one output, if applicable. Returns `true` when the
/// caller should `continue` past ordinary edge evaluation for this output (a `FanOut` spawning
/// its branches, or a *successful* branch recording itself into a `FanIn` barrier — complete or
/// not); returns `false` for every other node, including a `FanIn` itself once reached as an
/// active node (its outgoing `Always` edges go through the normal edge-evaluation path) and a
/// *failed* branch (a failed branch is an ordinary failed `Task`: only its `Condition::Failed`
/// edges may fire, and with none the whole execution hard-fails — folding an `Err` into the
/// barrier's `results` would be indistinguishable from "not reported yet" and stall it forever).
fn handle_fan(
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
            // The barrier's expected size is fixed here, at spawn time, and never recomputed:
            // re-reading `source` when the first branch happens to complete would see whatever
            // state another output in the same batch left behind (e.g. a `Replace` shrinking the
            // very array we fanned out over), sizing `results` too small for the branches that
            // were actually spawned.
            open_fan_in_barrier(graph, execution, target, branch_count);
            return true; // FanOut has no ordinary outgoing edges to evaluate
        }
        Some(Node::FanIn { .. }) => {
            // Reached only once its bookkeeping says every branch is in — ordinary edge
            // evaluation after this match handles its outgoing edges.
            return false;
        }
        _ => {}
    }

    if let Some(fan_in_node) = fan_in_consuming(graph, &output.node.node) {
        if output.result.is_err() {
            return false; // ordinary failed-Task handling: Failed edge, or hard-fail
        }
        record_fan_in_branch(execution, output, fan_in_node, next);
        return true;
    }

    false
}

/// Sizes (or resizes, on a second pass through the same `FanOut`) the `FanIn` barrier that
/// `branch_target`'s branches feed, storing `expected` `Null` slots into
/// `state["__fan_in"][fan_in_id]["results"]` the moment the branches are spawned.
fn open_fan_in_barrier(
    graph: &Graph,
    execution: &mut Execution,
    branch_target: &crate::ids::NodeId,
    expected: usize,
) {
    let Some(Node::FanIn { id, .. }) = fan_in_consuming(graph, branch_target) else {
        return; // a FanOut whose target doesn't lead to a FanIn — Graph::validate rejects it
    };
    let fan_in_id = id.0.clone();
    execution
        .state
        .as_object_mut()
        .expect("state is an object")
        .entry("__fan_in")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .expect("__fan_in is an object")
        .insert(
            fan_in_id,
            json!({"results": vec![serde_json::Value::Null; expected]}),
        );
}

/// Records one successful branch's result into its `FanIn`'s barrier bookkeeping
/// (`state["__fan_in"][id]`, already sized by `open_fan_in_barrier` when the `FanOut` fired) and,
/// once every branch is in, folds the collected results into `state[key]` via `reducer` and
/// pushes the `FanIn` node itself into `next`.
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
        .entry("__fan_in")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .expect("__fan_in is an object");
    let slot = fan_in_table
        .entry(fan_in_id.0.clone())
        .or_insert_with(|| json!({"results": []}));
    // The barrier was sized at spawn time, so `branch_index` is normally already in range; the
    // grow-if-short guard only covers a barrier restored from a checkpoint written before that
    // sizing existed, and replaces what used to be an index-out-of-bounds panic.
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
        return; // this branch is recorded; the barrier isn't complete yet
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

/// Replaces any completed `FanIn` node in `next` with the targets of its own outgoing edges,
/// evaluated against `execution.state` right now. A `FanIn` has no `TaskExecutor` of its own —
/// once its barrier is satisfied it behaves like an ordinary node whose output already landed in
/// state, so its edges fire in the same tick rather than parking it for a future output.
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

/// The `FanIn` node whose sole predecessor edge is `node` — i.e. `node` is a `FanOut`'s `target`
/// and this returns that `FanOut`'s paired `FanIn`. `Graph::validate` (Task 9) rejects graphs
/// where a `FanOut` target's outgoing edges don't lead to exactly one `FanIn`, so `step()` can
/// assume this shape rather than re-checking it every tick.
fn fan_in_consuming<'a>(graph: &'a Graph, node: &crate::ids::NodeId) -> Option<&'a Node> {
    graph
        .edges_from(node)
        .find_map(|edge| graph.node(&edge.to))
        .filter(|candidate| matches!(candidate, Node::FanIn { .. }))
}

/// Records the `NodeCompleted`/`NodeFailed` event for one output, charges the execution's budget
/// for a completed `Task` (see `charge_budget`), and, for a successful `Task` output that
/// declares a `state_key`, writes it into `execution.state` via its reducer.
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

/// Evaluates every outgoing edge from `output`'s node and pushes the ones that fire into `next`.
/// A failed output only fires `Condition::Failed` edges; if none fire, records `hard_failure` so
/// the caller can fail the whole execution.
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

/// Charges one completed `Task` against the execution's `Budget`: its token usage (from the
/// output's own `{"usage": {"tokens_in", "tokens_out"}}` object, which `LlmTaskExecutor` fills in
/// from llm-router's `CompleteResponse`; a `Task` kind that reports no usage charges 0 tokens)
/// plus exactly one tool call, since a completed `Task` node is one action whatever its kind.
/// Wall time isn't charged here — `step()` is pure and synchronous and has no elapsed-time
/// signal for the work that produced these outputs; charging it needs the runtime to time the
/// `TaskExecutor` call and report it alongside the output, which no port does today.
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
    execution
        .budget
        .charge(tokens, 1, std::time::Duration::ZERO);
}

/// `Task.config["state_key"]` names the state key this node's output is written to; absent
/// means the output only drives edge conditions via `Failed`/`Always`, not `state`. Node kinds
/// besides `Task` don't take this path — Task 5/6 add their own state-writing rules.
fn state_key(config: &serde_json::Value) -> Option<&str> {
    config.get("state_key").and_then(serde_json::Value::as_str)
}

fn reducer_of(config: &serde_json::Value) -> Option<crate::graph::Reducer> {
    match config.get("reducer").and_then(serde_json::Value::as_str) {
        Some("Replace") | None => Some(crate::graph::Reducer::Replace),
        Some("Append") => Some(crate::graph::Reducer::Append),
        Some("Merge") => Some(crate::graph::Reducer::Merge),
        Some(_) => None,
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
        // The node's own output has nowhere to land in state unless a reducer key is declared
        // for it — that's `Task.config["state_key"]`/`Task.config["reducer"]`, wired in Step 3.
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

        // First branch finishes: FanIn isn't ready yet, execution stays Ready with only the
        // second branch outstanding — nothing to do for the runtime but wait for it.
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

        // Second branch finishes: FanIn now has both results and fires into End.
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
        // Regression: a branch's Err used to be folded into the barrier as `null`, which is
        // exactly what an unreported branch looks like — the barrier never filled and the
        // execution stalled silently instead of failing like any other Task with no Failed edge.
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
        // Regression: the barrier's expected size used to be recomputed from live state at
        // branch-completion time, so another output in the same batch replacing the fanned-out
        // array shrank `results` below the number of branches actually spawned — an
        // index-out-of-bounds panic on the branch whose index no longer fit.
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

        // One batch: an unrelated node empties `/items` *before* either branch is processed.
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

        // No output was supplied for the Subgraph node — step() alone can't start a child
        // execution (that needs Postgres), so it can only ever be reached with an empty
        // `outputs` the first time; the *service*'s tick loop is what notices an un-started
        // Subgraph node in current_nodes and calls StartExecution before the next tick (Task
        // 12). step() itself just parks it and emits the same Waiting event a Wait node would,
        // so a StreamEvents watcher sees "blocked on something" uniformly either way.
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
