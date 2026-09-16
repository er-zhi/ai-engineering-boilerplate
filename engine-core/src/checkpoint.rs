// A checkpoint: the execution's state and position after one super-step, written alongside its
// events in the same database transaction (see the engine service's tick loop). schema_version
// is a plain constant today — nothing to upgrade from yet, so a generic Versioned<T> wrapper
// would be premature; see the spec's "Что реально переиспользуется" table.

use serde::{Deserialize, Serialize};

use crate::execution::ActiveNode;
use crate::ids::ExecutionId;

pub const CHECKPOINT_SCHEMA_VERSION: u16 = 1;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub schema_version: u16,
    pub execution_id: ExecutionId,
    pub step: u32,
    pub state: serde_json::Value,
    pub current_nodes: Vec<ActiveNode>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn checkpoint_round_trips_through_json() {
        let checkpoint = Checkpoint {
            schema_version: CHECKPOINT_SCHEMA_VERSION,
            execution_id: ExecutionId(uuid::Uuid::new_v4()),
            step: 3,
            state: json!({"messages": ["hi"]}),
            current_nodes: vec![],
        };
        let encoded = serde_json::to_string(&checkpoint).expect("serialize");
        let decoded: Checkpoint = serde_json::from_str(&encoded).expect("deserialize");
        assert_eq!(checkpoint, decoded);
    }
}
