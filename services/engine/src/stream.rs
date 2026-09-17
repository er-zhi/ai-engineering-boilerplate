// Projects stored execution events onto the StreamEvents contract.

use std::time::Duration;

use buffa::EnumValue;
use common::proto::engine::v1::{ExecutionEvent as ProtoEvent, ExecutionEventKind};
use connectrpc::{ConnectError, ServiceStream};
use engine_core::ExecutionPayload;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder, QuerySelect};
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;

use crate::execution_event;

const CHANNEL_CAPACITY: usize = 64;
const POLL_BATCH: u64 = 100;
const POLL_INTERVAL: Duration = Duration::from_millis(200);
const POLL_FAILED: &str = "engine could not read the execution event log";
const EMPTY_PAYLOAD: &str = "{}";

const TERMINAL_KINDS: [ExecutionEventKind; 3] = [
    ExecutionEventKind::ExecutionCompleted,
    ExecutionEventKind::ExecutionFailed,
    ExecutionEventKind::ExecutionCancelled,
];

#[derive(Clone, Copy, Debug)]
pub enum Scope {
    Execution(Uuid),
    User(Uuid),
}

impl Scope {
    fn filter(
        self,
        query: sea_orm::Select<execution_event::Entity>,
    ) -> sea_orm::Select<execution_event::Entity> {
        match self {
            Scope::Execution(id) => query.filter(execution_event::Column::ExecutionId.eq(id)),
            Scope::User(id) => query.filter(execution_event::Column::UserId.eq(id)),
        }
    }

    fn closes_on(self, kind: EnumValue<ExecutionEventKind>) -> bool {
        matches!(self, Scope::Execution(_))
            && kind
                .as_known()
                .is_some_and(|known| TERMINAL_KINDS.contains(&known))
    }
}

#[must_use]
pub fn stream_events(db: DatabaseConnection, scope: Scope) -> ServiceStream<ProtoEvent> {
    let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
    tokio::spawn(async move { poll_forever(db, scope, tx).await });
    Box::pin(ReceiverStream::new(rx))
}

async fn poll_forever(
    db: DatabaseConnection,
    scope: Scope,
    tx: mpsc::Sender<Result<ProtoEvent, ConnectError>>,
) {
    let mut last_id: i64 = 0;
    loop {
        let query = scope.filter(
            execution_event::Entity::find().filter(execution_event::Column::Id.gt(last_id)),
        );
        let rows = match query
            .order_by_asc(execution_event::Column::Id)
            .limit(POLL_BATCH)
            .all(&db)
            .await
        {
            Ok(rows) => rows,
            Err(error) => {
                tracing::error!(%error, "StreamEvents poll failed");
                send_or_stop(&tx, Err(ConnectError::internal(POLL_FAILED))).await;
                return;
            }
        };
        if rows.is_empty() {
            tokio::time::sleep(POLL_INTERVAL).await;
            continue;
        }
        for row in rows {
            last_id = row.id;
            let event = row_to_proto(row);
            let closes = scope.closes_on(event.payload_kind);
            if !send_or_stop(&tx, Ok(event)).await || closes {
                return;
            }
        }
    }
}

async fn send_or_stop(
    tx: &mpsc::Sender<Result<ProtoEvent, ConnectError>>,
    item: Result<ProtoEvent, ConnectError>,
) -> bool {
    match tx.send(item).await {
        Ok(()) => true,
        Err(_) => {
            tracing::debug!("StreamEvents subscriber disconnected");
            false
        }
    }
}

fn projection(payload: &ExecutionPayload) -> (ExecutionEventKind, Option<String>, Option<String>) {
    use ExecutionPayload as Stored;
    match payload {
        Stored::ExecutionStarted => (ExecutionEventKind::ExecutionStarted, None, None),
        Stored::NodeStarted { .. } => (ExecutionEventKind::NodeStarted, None, None),
        Stored::NodeCompleted { .. } => (ExecutionEventKind::NodeCompleted, None, None),
        Stored::NodeFailed { error, .. } => {
            (ExecutionEventKind::NodeFailed, None, Some(error.clone()))
        }
        Stored::TaskOutput { .. } => (ExecutionEventKind::TaskOutput, None, None),
        Stored::Waiting { .. } => (ExecutionEventKind::Waiting, None, None),
        Stored::Resumed => (ExecutionEventKind::Resumed, None, None),
        Stored::Interrupted => (ExecutionEventKind::Interrupted, None, None),
        Stored::ExecutionCompleted { result, .. } => (
            ExecutionEventKind::ExecutionCompleted,
            result.as_ref().map(as_text),
            None,
        ),
        Stored::ExecutionFailed { error } => (
            ExecutionEventKind::ExecutionFailed,
            None,
            Some(error.clone()),
        ),
        Stored::ExecutionCancelled => (ExecutionEventKind::ExecutionCancelled, None, None),
    }
}

fn as_text(value: &serde_json::Value) -> String {
    value
        .as_str()
        .map_or_else(|| value.to_string(), str::to_owned)
}

fn stored_fields(payload: &serde_json::Value) -> Option<&serde_json::Value> {
    match payload {
        serde_json::Value::Object(envelope) if envelope.len() == 1 => {
            envelope.values().next().filter(|fields| fields.is_object())
        }
        _ => None,
    }
}

fn row_to_proto(row: execution_event::Model) -> ProtoEvent {
    let stored = ExecutionPayload::deserialize(&row.payload)
        .inspect_err(|error| tracing::warn!(%error, "stored execution event has no declared kind"))
        .ok();
    let (kind, result, error) = stored
        .as_ref()
        .map_or((ExecutionEventKind::Unspecified, None, None), |payload| {
            projection(payload)
        });
    let fields = stored_fields(&row.payload);
    ProtoEvent {
        id: row.event_id.to_string(),
        execution_id: row.execution_id.to_string(),
        user_id: row.user_id.map(|id| id.to_string()),
        version: i32::from(row.version),
        causation_id: row.causation_id.map(|id| id.to_string()),
        occurred_at: row.occurred_at.to_rfc3339(),
        payload_kind: kind.into(),
        payload_json: fields.map_or_else(|| EMPTY_PAYLOAD.to_owned(), serde_json::Value::to_string),
        result,
        error,
        ..Default::default()
    }
}

#[cfg(test)]
mod projection_tests {
    use super::*;

    fn row(payload: serde_json::Value) -> execution_event::Model {
        execution_event::Model {
            id: 1,
            event_id: Uuid::new_v4(),
            execution_id: Uuid::new_v4(),
            user_id: None,
            version: 1,
            causation_id: None,
            occurred_at: chrono::Utc::now(),
            payload,
        }
    }

    fn stored(payload: &engine_core::ExecutionPayload) -> serde_json::Value {
        serde_json::to_value(payload).expect("serialize")
    }

    #[test]
    fn a_payload_with_fields_projects_to_its_kind_and_its_own_fields() {
        let event = row_to_proto(row(stored(&engine_core::ExecutionPayload::NodeCompleted {
            node: engine_core::NodeId("llm".to_owned()),
            output: serde_json::json!({"reply": "hi"}),
        })));

        assert_eq!(event.payload_kind, ExecutionEventKind::NodeCompleted);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&event.payload_json).expect("json"),
            serde_json::json!({"node": "llm", "output": {"reply": "hi"}}),
            "the variant's own fields, never an envelope keyed by the kind"
        );
        assert_eq!(event.result, None);
        assert_eq!(event.error, None);
    }

    #[test]
    fn a_payload_with_no_fields_projects_to_its_kind_and_an_empty_object() {
        let event = row_to_proto(row(stored(
            &engine_core::ExecutionPayload::ExecutionCancelled,
        )));

        assert_eq!(event.payload_kind, ExecutionEventKind::ExecutionCancelled);
        assert_eq!(event.payload_json, EMPTY_PAYLOAD);
    }

    #[test]
    fn a_completed_payload_carries_the_resolved_answer_as_the_result() {
        let event = row_to_proto(row(stored(
            &engine_core::ExecutionPayload::ExecutionCompleted {
                final_state: serde_json::json!({"llm": {"reply": "62F and foggy"}}),
                result: Some(serde_json::json!("62F and foggy")),
            },
        )));

        assert_eq!(event.payload_kind, ExecutionEventKind::ExecutionCompleted);
        assert_eq!(event.result.as_deref(), Some("62F and foggy"));
        assert_eq!(event.error, None);
    }

    #[test]
    fn a_completed_payload_whose_graph_declared_no_answer_carries_no_result() {
        let event = row_to_proto(row(stored(
            &engine_core::ExecutionPayload::ExecutionCompleted {
                final_state: serde_json::json!({"llm": {"reply": "62F and foggy"}}),
                result: None,
            },
        )));

        assert_eq!(event.result, None);
    }

    #[test]
    fn a_failing_payload_carries_its_error() {
        let failed = row_to_proto(row(stored(
            &engine_core::ExecutionPayload::ExecutionFailed {
                error: "brave search returned 422".to_owned(),
            },
        )));
        assert_eq!(failed.payload_kind, ExecutionEventKind::ExecutionFailed);
        assert_eq!(failed.error.as_deref(), Some("brave search returned 422"));

        let node_failed = row_to_proto(row(stored(&engine_core::ExecutionPayload::NodeFailed {
            node: engine_core::NodeId("tool".to_owned()),
            error: "tool timed out".to_owned(),
        })));
        assert_eq!(node_failed.payload_kind, ExecutionEventKind::NodeFailed);
        assert_eq!(node_failed.error.as_deref(), Some("tool timed out"));
    }

    #[test]
    fn a_row_written_before_the_result_field_existed_still_projects() {
        let event = row_to_proto(row(
            serde_json::json!({"ExecutionCompleted": {"final_state": {"llm": {"reply": "done"}}}}),
        ));

        assert_eq!(event.payload_kind, ExecutionEventKind::ExecutionCompleted);
        assert_eq!(event.result, None);
    }

    #[test]
    fn a_row_whose_kind_this_engine_does_not_know_is_unspecified_rather_than_empty() {
        for unknown in [
            serde_json::json!({"SomethingRenamed": {"node": "llm"}}),
            serde_json::json!("SomethingElseRenamed"),
            serde_json::json!([]),
        ] {
            let event = row_to_proto(row(unknown.clone()));
            assert_eq!(
                event.payload_kind,
                ExecutionEventKind::Unspecified,
                "{unknown} must not be silently dropped"
            );
        }
    }

    #[test]
    fn every_terminal_kind_closes_an_execution_scoped_subscription() {
        let scope = Scope::Execution(Uuid::new_v4());
        for payload in [
            engine_core::ExecutionPayload::ExecutionCompleted {
                final_state: serde_json::json!({}),
                result: None,
            },
            engine_core::ExecutionPayload::ExecutionFailed {
                error: "boom".to_owned(),
            },
            engine_core::ExecutionPayload::ExecutionCancelled,
        ] {
            let event = row_to_proto(row(stored(&payload)));
            assert!(
                scope.closes_on(event.payload_kind),
                "{:?} ends the execution",
                event.payload_kind
            );
        }

        let progress = row_to_proto(row(stored(&engine_core::ExecutionPayload::Resumed)));
        assert!(!scope.closes_on(progress.payload_kind));
    }

    #[test]
    fn a_user_scoped_subscription_is_closed_by_nothing() {
        let scope = Scope::User(Uuid::new_v4());
        let event = row_to_proto(row(stored(
            &engine_core::ExecutionPayload::ExecutionCancelled,
        )));

        assert!(!scope.closes_on(event.payload_kind));
    }
}

#[cfg(all(test, feature = "test-support"))]
mod tests {
    use super::*;
    use futures::StreamExt;
    use sea_orm::{ActiveModelTrait, ActiveValue::Set};

    async fn insert_event(db: &DatabaseConnection, execution_id: Uuid, payload: serde_json::Value) {
        execution_event::ActiveModel {
            event_id: Set(Uuid::new_v4()),
            execution_id: Set(execution_id),
            user_id: Set(None),
            version: Set(1),
            causation_id: Set(None),
            occurred_at: Set(chrono::Utc::now()),
            payload: Set(payload),
            ..Default::default()
        }
        .insert(db)
        .await
        .expect("insert event");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_stream_yields_events_in_order_and_closes_on_a_terminal_one() {
        let test = crate::test_db::start().await;
        let execution_id = Uuid::new_v4();
        insert_event(
            &test.db,
            execution_id,
            serde_json::json!({"NodeStarted": {"node": "llm"}}),
        )
        .await;
        insert_event(
            &test.db,
            execution_id,
            serde_json::json!({"ExecutionCompleted": {"final_state": {}}}),
        )
        .await;

        let mut stream = stream_events(test.db.clone(), Scope::Execution(execution_id));
        let first = stream.next().await.expect("first").expect("ok");
        assert_eq!(first.payload_kind, ExecutionEventKind::NodeStarted);
        let second = stream.next().await.expect("second").expect("ok");
        assert_eq!(second.payload_kind, ExecutionEventKind::ExecutionCompleted);
        assert!(
            stream.next().await.is_none(),
            "closes after a terminal event on an execution-scoped stream"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_user_scoped_stream_does_not_close_on_a_terminal_event() {
        let test = crate::test_db::start().await;
        let user_id = Uuid::new_v4();
        let event = execution_event::ActiveModel {
            event_id: Set(Uuid::new_v4()),
            execution_id: Set(Uuid::new_v4()),
            user_id: Set(Some(user_id)),
            version: Set(1),
            causation_id: Set(None),
            occurred_at: Set(chrono::Utc::now()),
            payload: Set(serde_json::json!({"ExecutionCompleted": {"final_state": {}}})),
            ..Default::default()
        };
        event.insert(&test.db).await.expect("insert");

        let mut stream = stream_events(test.db.clone(), Scope::User(user_id));
        let first = stream.next().await.expect("first").expect("ok");
        assert_eq!(first.payload_kind, ExecutionEventKind::ExecutionCompleted);
    }
}
