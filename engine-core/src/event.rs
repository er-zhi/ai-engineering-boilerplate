// Defines the append-only record of everything that happens to an execution.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::graph::WaitKind;
use crate::ids::{ExecutionId, NodeId, UserId};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Event<P> {
    pub id: Uuid,
    pub version: u16,
    pub occurred_at: DateTime<Utc>,
    pub user_id: Option<UserId>,
    pub correlation_id: ExecutionId,
    pub causation_id: Option<Uuid>,
    pub payload: P,
}

pub type ExecutionEvent = Event<ExecutionPayload>;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ExecutionPayload {
    ExecutionStarted,
    NodeStarted {
        node: NodeId,
    },
    NodeCompleted {
        node: NodeId,
        output: serde_json::Value,
    },
    NodeFailed {
        node: NodeId,
        error: String,
    },
    TaskOutput {
        node: NodeId,
        chunk: serde_json::Value,
    },
    Waiting {
        on: WaitKind,
    },
    Resumed,
    Interrupted,
    ExecutionCompleted {
        final_state: serde_json::Value,
        #[serde(default)]
        result: Option<serde_json::Value>,
    },
    ExecutionFailed {
        error: String,
    },
    ExecutionCancelled,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn event_round_trips_through_json() {
        let event = Event {
            id: Uuid::new_v4(),
            version: 1,
            occurred_at: Utc::now(),
            user_id: None,
            correlation_id: ExecutionId(Uuid::new_v4()),
            causation_id: None,
            payload: ExecutionPayload::NodeCompleted {
                node: NodeId("llm".to_owned()),
                output: json!({"text": "hi"}),
            },
        };
        let encoded = serde_json::to_string(&event).expect("serialize");
        let decoded: ExecutionEvent = serde_json::from_str(&encoded).expect("deserialize");
        assert_eq!(event, decoded);
    }
}
