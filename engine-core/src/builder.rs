// GraphBuilder: assembles a Graph node-by-node/edge-by-edge in Rust, producing exactly the JSON
// RegisterGraph would otherwise accept from a client — built-in graphs have no second format.

use crate::graph::{Condition, Edge, Graph, Node, WaitKind};
use crate::ids::{GraphId, NodeId};

#[derive(Default)]
pub struct GraphBuilder {
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    entry: Option<NodeId>,
}

impl GraphBuilder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn entry(mut self, id: &str) -> Self {
        self.entry = Some(NodeId(id.to_owned()));
        self
    }

    #[must_use]
    pub fn task(mut self, id: &str, kind: &str, config: serde_json::Value) -> Self {
        self.nodes.push(Node::Task {
            id: NodeId(id.to_owned()),
            kind: kind.to_owned(),
            config,
        });
        self
    }

    #[must_use]
    pub fn fan_out(mut self, id: &str, source: &str, item_var: &str, target: &str) -> Self {
        self.nodes.push(Node::FanOut {
            id: NodeId(id.to_owned()),
            source: source.to_owned(),
            item_var: item_var.to_owned(),
            target: NodeId(target.to_owned()),
        });
        self
    }

    #[must_use]
    pub fn fan_in(mut self, id: &str, key: &str, reducer: crate::graph::Reducer) -> Self {
        self.nodes.push(Node::FanIn {
            id: NodeId(id.to_owned()),
            key: key.to_owned(),
            reducer,
        });
        self
    }

    #[must_use]
    pub fn subgraph(
        mut self,
        id: &str,
        graph_id: &str,
        version: Option<u32>,
        input: &str,
        output_key: &str,
    ) -> Self {
        self.nodes.push(Node::Subgraph {
            id: NodeId(id.to_owned()),
            graph_id: GraphId(graph_id.to_owned()),
            version,
            input: input.to_owned(),
            output_key: output_key.to_owned(),
        });
        self
    }

    #[must_use]
    pub fn wait(mut self, id: &str, on: WaitKind) -> Self {
        self.nodes.push(Node::Wait {
            id: NodeId(id.to_owned()),
            on,
        });
        self
    }

    #[must_use]
    pub fn end(mut self, id: &str) -> Self {
        self.nodes.push(Node::End {
            id: NodeId(id.to_owned()),
        });
        self
    }

    #[must_use]
    pub fn edge(mut self, from: &str, to: &str, condition: Condition) -> Self {
        self.edges.push(Edge {
            from: NodeId(from.to_owned()),
            to: NodeId(to.to_owned()),
            condition,
        });
        self
    }

    #[must_use]
    pub fn build(self, id: &str, version: u32) -> Graph {
        Graph {
            id: GraphId(id.to_owned()),
            version,
            user_id: None,
            nodes: self.nodes,
            edges: self.edges,
            entry: self
                .entry
                .expect("GraphBuilder::entry must be called before build"),
        }
    }
}

/// `START(entry=llm) → llm → End`.
///
/// `kind = "llm"`'s output is always `{"tool_call": {name, args} | null, "reply": string | null}`
/// — llm-router's `CompleteResponse` is plain `content: String` (see
/// `common/proto/llm_router/v1/llm_router.proto`, no native tool-calling), so Task 14's adapter
/// gets there by asking the model to answer in this JSON shape and parsing `content` as JSON;
/// every built-in graph below writes that whole object to state under the key `"llm"`, so the
/// final answer is always at `state["llm"]["reply"]` once an execution reaches `Completed`,
/// regardless of which built-in graph ran.
#[must_use]
pub fn simple_graph() -> Graph {
    GraphBuilder::new()
        .entry("llm")
        .task(
            "llm",
            "llm",
            serde_json::json!({"state_key": "llm", "reducer": "Replace"}),
        )
        .end("end")
        .edge("llm", "end", Condition::Always)
        .build("simple", 1)
}

/// `START(entry=kb_search) → kb_search → llm → End`.
#[must_use]
pub fn rag_graph() -> Graph {
    GraphBuilder::new()
        .entry("kb_search")
        .task(
            "kb_search",
            "tool",
            serde_json::json!({"state_key": "kb_results", "reducer": "Replace", "tool_slug": "kb_search"}),
        )
        .task(
            "llm",
            "llm",
            serde_json::json!({"state_key": "llm", "reducer": "Replace"}),
        )
        .end("end")
        .edge("kb_search", "llm", Condition::Always)
        .edge("llm", "end", Condition::Always)
        .build("rag", 1)
}

/// `START(entry=llm) → llm →[Truthy(/llm/tool_call)] tool → llm`,
/// `llm →[Not(Truthy(/llm/tool_call))] End` — the graph today's end-to-end scenario runs on. The
/// `tool` node's own `config` is empty — Task 15's tool `TaskExecutor` reads what to call from
/// `state["llm"]["tool_call"]` by the convention this graph and that executor share, not from
/// `config`, since `state` (unlike `config`) is already passed to every `TaskExecutor::execute`
/// call.
#[must_use]
pub fn agent_graph() -> Graph {
    GraphBuilder::new()
        .entry("llm")
        .task(
            "llm",
            "llm",
            serde_json::json!({"state_key": "llm", "reducer": "Replace"}),
        )
        // Append, not Replace: a topic can call more than one tool across loop iterations (the
        // day's own scenario asks for one integration call and one kb_search/web_search per
        // topic) — Replace would let the second tool result overwrite the first's before the
        // LLM ever sees both. state["tool_result"] is therefore an array the adapter folds its
        // whole prompt context from, not a single latest value.
        .task(
            "tool",
            "tool",
            serde_json::json!({"state_key": "tool_result", "reducer": "Append"}),
        )
        .end("end")
        .edge("llm", "tool", Condition::Truthy("/llm/tool_call".to_owned()))
        .edge(
            "llm",
            "end",
            Condition::Not(Box::new(Condition::Truthy("/llm/tool_call".to_owned()))),
        )
        .edge("tool", "llm", Condition::Always)
        .build("agent", 1)
}

// Note agent_graph's `tool → llm` edge always fires unconditionally, forming the cycle —
// combined with `max_iterations`, this is exactly `START → LLM ⇄ Tool → END` from the spec.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_graph_loops_llm_and_tool_until_no_tool_call() {
        let g = agent_graph();
        assert_eq!(g.entry, crate::ids::NodeId("llm".into()));
        assert!(g.node(&crate::ids::NodeId("tool".into())).is_some());
        assert!(g.node(&crate::ids::NodeId("end".into())).is_some());
    }
}
