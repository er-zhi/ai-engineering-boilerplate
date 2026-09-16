// The graph itself: Graph, Node, Edge, Condition. A Graph is plain data — JSON in, JSON out —
// so RegisterGraph can store exactly what GraphBuilder produces, with no second format for
// built-in graphs. Condition is a closed set over RFC 6901 JSON Pointers, not a scripting
// language: step() is synchronous and pure, so a condition can only ever inspect state already
// computed this super-step, never perform I/O.

use serde::{Deserialize, Serialize};

use crate::ids::{GraphId, NodeId};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Graph {
    pub id: GraphId,
    pub version: u32,
    pub user_id: Option<crate::ids::UserId>,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub entry: NodeId,
}

impl Graph {
    /// The node's own definition, by id. Returns `None` for an id the graph doesn't contain —
    /// callers treat that as a malformed graph, caught by `validate()` at registration time.
    #[must_use]
    pub fn node(&self, id: &NodeId) -> Option<&Node> {
        self.nodes.iter().find(|node| node.id() == id)
    }

    /// Every edge leading out of `from`.
    pub fn edges_from<'a>(&'a self, from: &'a NodeId) -> impl Iterator<Item = &'a Edge> {
        self.edges.iter().filter(move |edge| &edge.from == from)
    }

    /// Every structural problem found, not just the first — `RegisterGraph` calls this once,
    /// `step()` trusts a registered graph is valid and never re-checks these at runtime.
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut problems = Vec::new();
        let known: std::collections::HashSet<&NodeId> = self.nodes.iter().map(Node::id).collect();

        if !known.contains(&self.entry) {
            problems.push(format!("entry node {} is not declared", self.entry));
        }
        for edge in &self.edges {
            if !known.contains(&edge.from) {
                problems.push(format!("edge from unknown node {}", edge.from));
            }
            if !known.contains(&edge.to) {
                problems.push(format!("edge to unknown node {}", edge.to));
            }
        }
        for node in &self.nodes {
            if let Node::FanOut { target, .. } = node {
                let fan_ins: Vec<_> = self
                    .edges_from(target)
                    .filter(|edge| matches!(self.node(&edge.to), Some(Node::FanIn { .. })))
                    .collect();
                if fan_ins.len() != 1 {
                    problems.push(format!(
                        "FanOut target {target} must lead to exactly one FanIn, found {}",
                        fan_ins.len()
                    ));
                }
            }
        }

        if problems.is_empty() {
            Ok(())
        } else {
            Err(problems)
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Node {
    Task {
        id: NodeId,
        kind: String,
        config: serde_json::Value,
    },
    FanOut {
        id: NodeId,
        source: String, // JSON Pointer to an array in state
        item_var: String,
        target: NodeId,
    },
    FanIn {
        id: NodeId,
        key: String, // where the folded array of branch results is written in state
        reducer: Reducer,
    },
    Subgraph {
        id: NodeId,
        graph_id: GraphId,
        version: Option<u32>,
        input: String, // JSON Pointer to the value passed as the child execution's input
        output_key: String,
    },
    Wait {
        id: NodeId,
        on: WaitKind,
    },
    End {
        id: NodeId,
    },
}

impl Node {
    #[must_use]
    pub fn id(&self) -> &NodeId {
        match self {
            Node::Task { id, .. }
            | Node::FanOut { id, .. }
            | Node::FanIn { id, .. }
            | Node::Subgraph { id, .. }
            | Node::Wait { id, .. }
            | Node::End { id, .. } => id,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum WaitKind {
    Approval {
        risk: String,
    },
    ExternalEvent {
        key: String,
    },
    UserInput,
    Timer {
        until: chrono::DateTime<chrono::Utc>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Reducer {
    Replace,
    Append,
    Merge,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Edge {
    pub from: NodeId,
    pub to: NodeId,
    pub condition: Condition,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Condition {
    Always,
    Truthy(String),
    Eq(String, serde_json::Value),
    Exists(String),
    Failed,
    Not(Box<Condition>),
    And(Vec<Condition>),
    Or(Vec<Condition>),
}

/// Evaluates a condition against `state`. `Condition::Failed` is handled by the caller (`step`),
/// not here — this function only ever sees the state a node produced, never whether it errored,
/// so it always returns `false` for `Failed` and `step` special-cases the failed-node path
/// before consulting conditions at all.
#[must_use]
pub fn evaluate_condition(condition: &Condition, state: &serde_json::Value) -> bool {
    match condition {
        Condition::Always => true,
        Condition::Truthy(pointer) => state.pointer(pointer).is_some_and(is_truthy),
        Condition::Eq(pointer, expected) => state
            .pointer(pointer)
            .is_some_and(|value| value == expected),
        Condition::Exists(pointer) => state.pointer(pointer).is_some(),
        Condition::Failed => false,
        Condition::Not(inner) => !evaluate_condition(inner, state),
        Condition::And(parts) => parts.iter().all(|part| evaluate_condition(part, state)),
        Condition::Or(parts) => parts.iter().any(|part| evaluate_condition(part, state)),
    }
}

fn is_truthy(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Null => false,
        serde_json::Value::Bool(b) => *b,
        serde_json::Value::Number(n) => n.as_f64().is_some_and(|n| n != 0.0),
        serde_json::Value::String(s) => !s.is_empty(),
        serde_json::Value::Array(a) => !a.is_empty(),
        serde_json::Value::Object(o) => !o.is_empty(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn truthy_reads_a_json_pointer() {
        let state = json!({"tool_call": {"name": "web_search"}});
        assert!(evaluate_condition(
            &Condition::Truthy("/tool_call".to_owned()),
            &state
        ));

        let empty = json!({"tool_call": null});
        assert!(!evaluate_condition(
            &Condition::Truthy("/tool_call".to_owned()),
            &empty
        ));
    }

    #[test]
    fn eq_compares_the_pointed_value() {
        let state = json!({"status": "done"});
        assert!(evaluate_condition(
            &Condition::Eq("/status".to_owned(), json!("done")),
            &state
        ));
        assert!(!evaluate_condition(
            &Condition::Eq("/status".to_owned(), json!("pending")),
            &state
        ));
    }

    #[test]
    fn exists_is_true_only_when_the_pointer_resolves() {
        let state = json!({"a": {"b": 1}});
        assert!(evaluate_condition(
            &Condition::Exists("/a/b".to_owned()),
            &state
        ));
        assert!(!evaluate_condition(
            &Condition::Exists("/a/c".to_owned()),
            &state
        ));
    }

    #[test]
    fn not_and_and_or_compose() {
        let state = json!({"x": true, "y": false});
        assert!(evaluate_condition(
            &Condition::Not(Box::new(Condition::Truthy("/y".to_owned()))),
            &state
        ));
        assert!(evaluate_condition(
            &Condition::And(vec![
                Condition::Truthy("/x".to_owned()),
                Condition::Not(Box::new(Condition::Truthy("/y".to_owned())))
            ]),
            &state
        ));
        assert!(evaluate_condition(
            &Condition::Or(vec![Condition::Truthy("/y".to_owned()), Condition::Always]),
            &state
        ));
    }

    #[test]
    fn always_is_always_true() {
        assert!(evaluate_condition(&Condition::Always, &json!(null)));
    }

    #[test]
    fn validate_rejects_an_edge_to_an_unknown_node() {
        let g = Graph {
            id: crate::ids::GraphId("bad".into()),
            version: 1,
            user_id: None,
            nodes: vec![Node::End {
                id: NodeId("end".into()),
            }],
            edges: vec![Edge {
                from: NodeId("end".into()),
                to: NodeId("nowhere".into()),
                condition: Condition::Always,
            }],
            entry: NodeId("end".into()),
        };
        assert!(g.validate().is_err());
    }

    #[test]
    fn validate_rejects_a_fan_out_target_with_more_than_one_fan_in_downstream() {
        // A FanOut target must lead to exactly one FanIn — otherwise "which barrier does this
        // branch's result belong to" has no single answer.
        let g = Graph {
            id: crate::ids::GraphId("bad".into()),
            version: 1,
            user_id: None,
            nodes: vec![
                Node::FanOut {
                    id: NodeId("spread".into()),
                    source: "/items".into(),
                    item_var: "item".into(),
                    target: NodeId("work".into()),
                },
                Node::Task {
                    id: NodeId("work".into()),
                    kind: "noop".into(),
                    config: serde_json::json!({}),
                },
                Node::FanIn {
                    id: NodeId("a".into()),
                    key: "x".into(),
                    reducer: Reducer::Append,
                },
                Node::FanIn {
                    id: NodeId("b".into()),
                    key: "y".into(),
                    reducer: Reducer::Append,
                },
            ],
            edges: vec![
                Edge {
                    from: NodeId("work".into()),
                    to: NodeId("a".into()),
                    condition: Condition::Always,
                },
                Edge {
                    from: NodeId("work".into()),
                    to: NodeId("b".into()),
                    condition: Condition::Always,
                },
            ],
            entry: NodeId("spread".into()),
        };
        assert!(g.validate().is_err());
    }

    #[test]
    fn validate_accepts_the_three_built_in_graphs() {
        assert!(crate::builder::simple_graph().validate().is_ok());
        assert!(crate::builder::rag_graph().validate().is_ok());
        assert!(crate::builder::agent_graph().validate().is_ok());
    }
}
