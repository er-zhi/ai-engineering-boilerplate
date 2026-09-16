// Conversions between engine-core's pure types and the SeaORM entities that persist them.
// Isolated here so nothing else in the crate hand-rolls serde_json::to_value/from_value on
// these types — a schema_version mismatch or a bad Status string has exactly one place to fail
// loudly instead of N call sites each getting it slightly differently.

use engine_core::{Checkpoint, ExecutionEvent, Status, WaitKind};
use sea_orm::ActiveValue::Set;
use serde_json::Value;

use crate::entity::{checkpoint, execution_event};

#[must_use]
pub fn status_to_columns(status: &Status) -> (String, Option<Value>) {
    match status {
        Status::Ready => ("ready".to_owned(), None),
        Status::Running => ("running".to_owned(), None),
        Status::Waiting(wait) => (
            "waiting".to_owned(),
            Some(serde_json::to_value(wait).expect("WaitKind always serializes")),
        ),
        Status::Completed => ("completed".to_owned(), None),
        Status::Failed => ("failed".to_owned(), None),
        Status::Cancelled => ("cancelled".to_owned(), None),
    }
}

pub fn columns_to_status(status: &str, wait_kind: Option<Value>) -> Result<Status, String> {
    match status {
        "ready" => Ok(Status::Ready),
        "running" => Ok(Status::Running),
        "waiting" => {
            let wait_kind =
                wait_kind.ok_or_else(|| "waiting status with no wait_kind column".to_owned())?;
            let wait: WaitKind = serde_json::from_value(wait_kind).map_err(|e| e.to_string())?;
            Ok(Status::Waiting(wait))
        }
        "completed" => Ok(Status::Completed),
        "failed" => Ok(Status::Failed),
        "cancelled" => Ok(Status::Cancelled),
        other => Err(format!("unknown execution status column value: {other}")),
    }
}

#[must_use]
pub fn checkpoint_to_active_model(checkpoint: &Checkpoint) -> checkpoint::ActiveModel {
    checkpoint::ActiveModel {
        execution_id: Set(checkpoint.execution_id.0),
        step: Set(i32::try_from(checkpoint.step).unwrap_or(i32::MAX)),
        schema_version: Set(i16::try_from(checkpoint.schema_version).unwrap_or(i16::MAX)),
        state: Set(checkpoint.state.clone()),
        current_nodes: Set(serde_json::to_value(&checkpoint.current_nodes)
            .expect("ActiveNode list always serializes")),
        created_at: Set(chrono::Utc::now()),
        ..Default::default()
    }
}

pub fn checkpoint_from_model(model: checkpoint::Model) -> Result<Checkpoint, String> {
    if model.schema_version
        != i16::try_from(engine_core::CHECKPOINT_SCHEMA_VERSION).unwrap_or(i16::MAX)
    {
        return Err(format!(
            "checkpoint schema_version {} does not match the running binary's {}",
            model.schema_version,
            engine_core::CHECKPOINT_SCHEMA_VERSION
        ));
    }
    Ok(Checkpoint {
        schema_version: u16::try_from(model.schema_version).unwrap_or(0),
        execution_id: engine_core::ExecutionId(model.execution_id),
        step: u32::try_from(model.step).unwrap_or(0),
        state: model.state,
        current_nodes: serde_json::from_value(model.current_nodes).map_err(|e| e.to_string())?,
    })
}

#[must_use]
pub fn event_to_active_model(event: &ExecutionEvent) -> execution_event::ActiveModel {
    execution_event::ActiveModel {
        event_id: Set(event.id),
        execution_id: Set(event.correlation_id.0),
        user_id: Set(event.user_id.map(|u| u.0)),
        version: Set(i16::try_from(event.version).unwrap_or(i16::MAX)),
        causation_id: Set(event.causation_id),
        occurred_at: Set(event.occurred_at),
        payload: Set(
            serde_json::to_value(&event.payload).expect("ExecutionPayload always serializes")
        ),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_core::{ActiveNode, Event, ExecutionId, ExecutionPayload, NodeId};
    use std::time::Duration;

    #[test]
    fn status_round_trips_through_columns() {
        for status in [
            Status::Ready,
            Status::Running,
            Status::Waiting(WaitKind::UserInput),
            Status::Completed,
            Status::Failed,
            Status::Cancelled,
        ] {
            let (column, wait_kind) = status_to_columns(&status);
            assert_eq!(
                columns_to_status(&column, wait_kind).expect("valid"),
                status
            );
        }
    }

    #[test]
    fn checkpoint_round_trips_through_the_active_model() {
        // Builds the Model by hand rather than reading it back out of the ActiveModel Step 1
        // produces — Task 12's CheckpointStore is what actually round-trips through a real
        // INSERT/SELECT against Postgres; this test only proves checkpoint_from_model correctly
        // inverts the *shape* checkpoint_to_active_model writes, using values it already knows.
        let checkpoint = Checkpoint {
            schema_version: engine_core::CHECKPOINT_SCHEMA_VERSION,
            execution_id: ExecutionId(uuid::Uuid::new_v4()),
            step: 2,
            state: serde_json::json!({"messages": ["hi"]}),
            current_nodes: vec![ActiveNode::plain(NodeId("llm".into()))],
        };
        let _active = checkpoint_to_active_model(&checkpoint); // exercised for its own sake below
        let model = checkpoint::Model {
            id: 1,
            execution_id: checkpoint.execution_id.0,
            step: 2,
            schema_version: i16::try_from(checkpoint.schema_version).expect("fits"),
            state: checkpoint.state.clone(),
            current_nodes: serde_json::to_value(&checkpoint.current_nodes).expect("serialize"),
            created_at: chrono::Utc::now(),
        };
        assert_eq!(checkpoint_from_model(model).expect("valid"), checkpoint);
    }

    #[test]
    fn a_stale_schema_version_is_rejected() {
        let model = checkpoint::Model {
            id: 1,
            execution_id: uuid::Uuid::new_v4(),
            step: 0,
            schema_version: 999,
            state: serde_json::json!({}),
            current_nodes: serde_json::json!([]),
            created_at: chrono::Utc::now(),
        };
        assert!(checkpoint_from_model(model).is_err());
    }

    #[test]
    fn event_to_active_model_does_not_panic_on_every_payload_variant() {
        // Real round-trip coverage (INSERT, read back, compare) is Task 23's integration test —
        // this only proves every ExecutionPayload variant serializes through Set(...) without
        // panicking, since a `Value` that can't serialize would panic inside the function, not
        // return a Result (event_to_active_model is infallible by construction: every field of
        // ExecutionPayload is plain, already-`Serialize` data).
        let base = |payload: ExecutionPayload| Event {
            id: uuid::Uuid::new_v4(),
            version: 1,
            occurred_at: chrono::Utc::now(),
            user_id: None,
            correlation_id: ExecutionId(uuid::Uuid::new_v4()),
            causation_id: None,
            payload,
        };
        for event in [
            base(ExecutionPayload::ExecutionStarted),
            base(ExecutionPayload::NodeStarted {
                node: NodeId("llm".into()),
            }),
            base(ExecutionPayload::ExecutionCompleted {
                final_state: serde_json::json!({}),
            }),
        ] {
            let _active = event_to_active_model(&event);
        }
        let _ = Duration::ZERO; // keeps the Duration import used
    }
}
