// Assembles graphs in Rust and defines the three built-in ones.

use crate::graph::{Condition, Edge, Graph, Node, Reducer, WaitKind};
use crate::ids::{GraphId, NodeId};
use crate::llm_output::{
    LLM_STATE_KEY, TOOL_RESULT_STATE_KEY, llm_reply_pointer, llm_tool_call_pointer,
};

#[derive(Default)]
pub struct GraphBuilder {
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    entry: Option<NodeId>,
    answer_pointer: Option<String>,
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
    pub fn answer(mut self, pointer: &str) -> Self {
        self.answer_pointer = Some(pointer.to_owned());
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
            answer_pointer: self.answer_pointer,
        }
    }
}

#[must_use]
pub fn simple_graph() -> Graph {
    GraphBuilder::new()
        .entry("llm")
        .task(
            "llm",
            "llm",
            serde_json::json!({"state_key": LLM_STATE_KEY, "reducer": Reducer::Replace}),
        )
        .end("end")
        .edge("llm", "end", Condition::Always)
        .answer(&llm_reply_pointer())
        .build("simple", 1)
}

#[must_use]
pub fn rag_graph(knowledge_search_tool_slug: &str) -> Graph {
    GraphBuilder::new()
        .entry("retrieve")
        .task(
            "retrieve",
            "tool",
            serde_json::json!({
                "state_key": "kb_results",
                "reducer": Reducer::Replace,
                "tool_slug": knowledge_search_tool_slug,
            }),
        )
        .task(
            "llm",
            "llm",
            serde_json::json!({"state_key": LLM_STATE_KEY, "reducer": Reducer::Replace}),
        )
        .end("end")
        .edge("retrieve", "llm", Condition::Always)
        .edge("llm", "end", Condition::Always)
        .answer(&llm_reply_pointer())
        .build("rag", 1)
}

#[must_use]
pub fn agent_graph() -> Graph {
    GraphBuilder::new()
        .entry("llm")
        .task(
            "llm",
            "llm",
            serde_json::json!({
                "state_key": LLM_STATE_KEY,
                "reducer": Reducer::Replace,
                "tool_calling": true,
            }),
        )
        .task(
            "tool",
            "tool",
            serde_json::json!({"state_key": TOOL_RESULT_STATE_KEY, "reducer": Reducer::Append}),
        )
        .end("end")
        .edge("llm", "tool", Condition::Truthy(llm_tool_call_pointer()))
        .edge(
            "llm",
            "end",
            Condition::Not(Box::new(Condition::Truthy(llm_tool_call_pointer()))),
        )
        .edge("tool", "llm", Condition::Always)
        .answer(&llm_reply_pointer())
        .build("agent", 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::budget::Budget;
    use crate::execution::{ActiveNode, Execution, Status};
    use crate::graph::Node;
    use crate::ids::{ExecutionId, NodeId};
    use crate::step::{NodeOutput, step};
    use serde_json::json;

    #[test]
    fn rag_graph_retrieves_through_the_tool_slug_its_caller_supplies() {
        let g = rag_graph("whatever_the_catalog_calls_it");

        let Some(Node::Task { config, .. }) = g.node(&NodeId("retrieve".into())) else {
            panic!("retrieve node is a Task");
        };
        assert_eq!(config["tool_slug"], json!("whatever_the_catalog_calls_it"));
    }

    #[test]
    fn every_built_in_graph_declares_where_its_answer_lives() {
        for g in [simple_graph(), rag_graph("any_search_slug"), agent_graph()] {
            assert_eq!(
                g.answer(&json!({"llm": {"reply": "the answer"}})),
                Some(json!("the answer")),
                "{} must resolve its own answer",
                g.id
            );
        }
    }

    #[test]
    fn agent_graph_loops_llm_and_tool_until_no_tool_call() {
        let g = agent_graph();
        assert_eq!(g.entry, crate::ids::NodeId("llm".into()));
        assert!(g.node(&crate::ids::NodeId("tool".into())).is_some());
        assert!(g.node(&crate::ids::NodeId("end".into())).is_some());
    }

    fn agent_execution() -> Execution {
        Execution {
            id: ExecutionId(uuid::Uuid::new_v4()),
            graph_id: GraphId("agent".to_owned()),
            graph_version: 1,
            user_id: None,
            status: Status::Ready,
            current_nodes: vec![ActiveNode::plain(NodeId("llm".to_owned()))],
            state: json!({"question": "weather in SF?"}),
            iteration: 0,
            max_iterations: 20,
            deadline: None,
            budget: Budget::new(100_000, 100, std::time::Duration::from_secs(3600)),
        }
    }

    fn output(node: &str, result: Result<serde_json::Value, String>) -> Vec<NodeOutput> {
        vec![NodeOutput {
            node: ActiveNode::plain(NodeId(node.to_owned())),
            result,
        }]
    }

    #[test]
    fn a_tool_error_observation_feeds_the_loop_instead_of_failing_the_execution() {
        let g = agent_graph();
        let exec = agent_execution();
        let now = chrono::Utc::now();

        let (exec, _) = step(
            &g,
            exec,
            output(
                "llm",
                Ok(json!({"tool_call": {"name": "web_fetch"}, "reply": null})),
            ),
            now,
        );
        assert_eq!(
            exec.current_nodes,
            vec![ActiveNode::plain(NodeId("tool".to_owned()))]
        );

        let (exec, _) = step(
            &g,
            exec,
            output(
                "tool",
                Ok(json!({"error": "error sending request for url"})),
            ),
            now,
        );

        assert_eq!(exec.status, Status::Ready);
        assert_eq!(
            exec.current_nodes,
            vec![ActiveNode::plain(NodeId("llm".to_owned()))]
        );
        assert_eq!(
            exec.state["tool_result"],
            json!([{"error": "error sending request for url"}])
        );
    }

    #[test]
    fn the_llm_answering_after_a_tool_error_completes_the_execution() {
        let g = agent_graph();
        let now = chrono::Utc::now();
        let mut exec = agent_execution();
        exec.state["tool_result"] = json!([{"error": "invalid_argument: query is required"}]);
        exec.current_nodes = vec![ActiveNode::plain(NodeId("llm".to_owned()))];

        let (exec, _) = step(
            &g,
            exec,
            output(
                "llm",
                Ok(json!({"tool_call": null, "reply": "It's about 62F and foggy."})),
            ),
            now,
        );

        assert_eq!(exec.status, Status::Completed);
        assert_eq!(
            exec.state["llm"]["reply"],
            json!("It's about 62F and foggy.")
        );
    }

    #[test]
    fn a_tool_node_failure_fails_the_execution_with_the_last_error_text() {
        let g = agent_graph();
        let now = chrono::Utc::now();
        let mut exec = agent_execution();
        exec.current_nodes = vec![ActiveNode::plain(NodeId("tool".to_owned()))];

        let (exec, events) = step(
            &g,
            exec,
            output(
                "tool",
                Err("3 consecutive tool errors, giving up; last error: error sending request for url (https://www.accuweather.com/)".to_owned()),
            ),
            now,
        );

        assert_eq!(exec.status, Status::Failed);
        assert!(events.iter().any(|event| matches!(
            &event.payload,
            crate::event::ExecutionPayload::ExecutionFailed { error }
                if error.contains("3 consecutive tool errors") && error.contains("accuweather.com")
        )));
    }
}
