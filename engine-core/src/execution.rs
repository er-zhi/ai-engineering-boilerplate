// One execution of one graph: current position (current_nodes), accumulated state, and the
// bookkeeping (iteration/budget/deadline) step() checks before doing more work. ActiveNode
// carries a branch index so a FanOut's spawned instances of the same node are distinguishable
// without inventing synthetic NodeIds the graph never declared.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::budget::Budget;
use crate::graph::WaitKind;
use crate::ids::{ExecutionId, GraphId, NodeId, UserId};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ActiveNode {
    pub node: NodeId,
    pub branch: Option<u32>,
}

impl ActiveNode {
    #[must_use]
    pub fn plain(node: NodeId) -> Self {
        Self { node, branch: None }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Execution {
    pub id: ExecutionId,
    pub graph_id: GraphId,
    pub graph_version: u32,
    pub user_id: Option<UserId>,
    pub status: Status,
    pub current_nodes: Vec<ActiveNode>,
    pub state: serde_json::Value,
    pub iteration: u32,
    pub max_iterations: u32,
    pub deadline: Option<DateTime<Utc>>,
    pub budget: Budget,
}

impl Execution {
    #[must_use]
    pub fn event(&self, payload: crate::event::ExecutionPayload) -> crate::event::ExecutionEvent {
        crate::event::Event {
            id: uuid::Uuid::new_v4(),
            version: 1,
            occurred_at: chrono::Utc::now(),
            user_id: self.user_id,
            correlation_id: self.id,
            causation_id: None,
            payload,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Status {
    Ready,
    Running,
    Waiting(WaitKind),
    Completed,
    Failed,
    Cancelled,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;

    fn sample() -> Execution {
        Execution {
            id: ExecutionId(uuid::Uuid::new_v4()),
            graph_id: GraphId("agent".to_owned()),
            graph_version: 1,
            user_id: None,
            status: Status::Ready,
            current_nodes: vec![ActiveNode::plain(NodeId("llm".to_owned()))],
            state: json!({}),
            iteration: 0,
            max_iterations: 10,
            deadline: None,
            budget: Budget::new(1000, 20, Duration::from_secs(60)),
        }
    }

    #[test]
    fn execution_round_trips_through_json() {
        let execution = sample();
        let encoded = serde_json::to_string(&execution).expect("serialize");
        let decoded: Execution = serde_json::from_str(&encoded).expect("deserialize");
        assert_eq!(execution, decoded);
    }

    #[test]
    fn a_fan_out_branch_carries_its_index() {
        let branch = ActiveNode {
            node: NodeId("summarize".to_owned()),
            branch: Some(2),
        };
        assert_eq!(branch.branch, Some(2));
        assert_eq!(ActiveNode::plain(NodeId("llm".to_owned())).branch, None);
    }
}
