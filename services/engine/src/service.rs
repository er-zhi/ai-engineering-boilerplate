// The three request handlers a fresh graph/execution needs: register a graph (validated,
// versioned), start an execution of it (seeds checkpoint 0 with the caller's input as state, so
// Task 15's tick loop has real state to load rather than an empty object), and read one back.
// main.rs's Connect trait impl only translates proto <-> these plain Rust signatures.

use chrono::Utc;
use engine_core::{
    ActiveNode, Budget, CHECKPOINT_SCHEMA_VERSION, Checkpoint, CheckpointStore, Graph,
};
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, ConnectionTrait, DatabaseConnection,
    EntityTrait, QueryFilter, QueryOrder, TransactionTrait,
};
use serde_json::Value;
use uuid::Uuid;

use crate::entity;
use crate::error::EngineError;
use crate::store::PgCheckpointStore;
use crate::wire;

const DEFAULT_MAX_ITERATIONS: i32 = 25;
const DEFAULT_TOKEN_BUDGET: u32 = 200_000;
const DEFAULT_TOOL_CALL_BUDGET: u32 = 50;
const DEFAULT_WALL_TIME_BUDGET: std::time::Duration = std::time::Duration::from_secs(600);

pub struct Service {
    db: DatabaseConnection,
}

impl Service {
    #[must_use]
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    pub async fn register_graph(
        &self,
        graph_id: String,
        definition_json: &str,
    ) -> Result<(String, i32), EngineError> {
        let definition: Graph = serde_json::from_str(definition_json)
            .map_err(|e| EngineError::InvalidGraph(e.to_string()))?;
        definition
            .validate()
            .map_err(|problems| EngineError::InvalidGraph(problems.join("; ")))?;

        let next_version = entity::graph::Entity::find()
            .filter(entity::graph::Column::GraphId.eq(graph_id.clone()))
            .order_by_desc(entity::graph::Column::Version)
            .one(&self.db)
            .await?
            .map_or(1, |row| row.version + 1);

        // The stored definition's own `version` field must match the row's `version` column —
        // whatever the caller sent in definition_json (GraphBuilder output always writes 1,
        // clients may send anything) is overwritten here, not trusted, so a later
        // deserialize-from-row round-trip never reads back a stale version.
        let mut definition = definition;
        definition.version = u32::try_from(next_version).unwrap_or(u32::MAX);

        entity::graph::ActiveModel {
            graph_id: Set(graph_id.clone()),
            version: Set(next_version),
            user_id: Set(None),
            definition: Set(serde_json::to_value(&definition)?),
            created_at: Set(Utc::now()),
            ..Default::default()
        }
        .insert(&self.db)
        .await?;

        Ok((graph_id, next_version))
    }

    pub async fn start_execution(
        &self,
        graph_id: String,
        version: Option<i32>,
        input_json: &str,
        user_id: Option<Uuid>,
    ) -> Result<Uuid, EngineError> {
        let input: Value = serde_json::from_str(input_json)
            .map_err(|e| EngineError::InvalidRequest(e.to_string()))?;
        // Execution state is an object by construction everywhere downstream — apply_reducer
        // (engine-core) unwraps `state.as_object_mut()` — so a scalar or array input has to be
        // rejected here, before a row exists, rather than panicking mid-tick later.
        if !input.is_object() {
            return Err(EngineError::InvalidRequest(format!(
                "input_json must decode to a JSON object, got: {input_json}"
            )));
        }

        let mut query = entity::graph::Entity::find()
            .filter(entity::graph::Column::GraphId.eq(graph_id.clone()));
        query = match version {
            Some(v) => query.filter(entity::graph::Column::Version.eq(v)),
            None => query.order_by_desc(entity::graph::Column::Version),
        };
        let graph_row = query
            .one(&self.db)
            .await?
            .ok_or_else(|| EngineError::GraphNotFound(graph_id.clone(), version))?;
        let graph: Graph = serde_json::from_value(graph_row.definition)?;

        let execution_id = Uuid::new_v4();
        let budget = Budget::new(
            DEFAULT_TOKEN_BUDGET,
            DEFAULT_TOOL_CALL_BUDGET,
            DEFAULT_WALL_TIME_BUDGET,
        );
        let entry_nodes = vec![ActiveNode::plain(graph.entry.clone())];

        entity::execution::ActiveModel {
            id: Set(execution_id),
            graph_id: Set(graph_row.graph_id),
            graph_version: Set(graph_row.version),
            user_id: Set(user_id),
            status: Set("ready".to_owned()),
            wait_kind: Set(None),
            current_nodes: Set(serde_json::to_value(&entry_nodes)?),
            iteration: Set(0),
            max_iterations: Set(DEFAULT_MAX_ITERATIONS),
            deadline: Set(None),
            budget: Set(serde_json::to_value(budget)?),
            lease_owner: Set(None),
            lease_until: Set(None),
            created_at: Set(Utc::now()),
            updated_at: Set(Utc::now()),
        }
        .insert(&self.db)
        .await?;

        PgCheckpointStore::new(self.db.clone())
            .save(&Checkpoint {
                schema_version: CHECKPOINT_SCHEMA_VERSION,
                execution_id: engine_core::ExecutionId(execution_id),
                step: 0,
                state: input,
                current_nodes: entry_nodes,
            })
            .await
            // CheckpointStore::save returns Result<(), String> (Task 9's port), not a DbErr —
            // wrapped into InvalidRequest rather than a new variant, since in practice it can
            // only fail on the same kind of database problem EngineError::Db already names.
            .map_err(EngineError::InvalidRequest)?;

        self.db.execute_unprepared("NOTIFY engine_tick").await?;
        Ok(execution_id)
    }

    pub async fn get_execution(
        &self,
        execution_id: Uuid,
    ) -> Result<engine_core::Execution, EngineError> {
        let row = entity::execution::Entity::find_by_id(execution_id)
            .one(&self.db)
            .await?
            .ok_or(EngineError::ExecutionNotFound(execution_id))?;
        let checkpoint = PgCheckpointStore::new(self.db.clone())
            .latest(engine_core::ExecutionId(execution_id))
            .await
            .map_err(EngineError::InvalidRequest)?;
        let (state, current_nodes) = checkpoint.map_or_else(
            || {
                (
                    serde_json::json!({}),
                    serde_json::from_value(row.current_nodes.clone()).unwrap_or_default(),
                )
            },
            |c| (c.state, c.current_nodes),
        );
        Ok(engine_core::Execution {
            id: engine_core::ExecutionId(execution_id),
            graph_id: engine_core::GraphId(row.graph_id),
            graph_version: u32::try_from(row.graph_version).unwrap_or(0),
            user_id: row.user_id.map(engine_core::UserId),
            status: wire::columns_to_status(&row.status, row.wait_kind)
                .map_err(EngineError::InvalidRequest)?,
            current_nodes,
            state,
            iteration: u32::try_from(row.iteration).unwrap_or(0),
            max_iterations: u32::try_from(row.max_iterations).unwrap_or(0),
            deadline: row.deadline,
            budget: serde_json::from_value(row.budget)?,
        })
    }

    async fn load(
        &self,
        execution_id: Uuid,
    ) -> Result<(entity::execution::Model, engine_core::Execution), EngineError> {
        let row = entity::execution::Entity::find_by_id(execution_id)
            .one(&self.db)
            .await?
            .ok_or(EngineError::ExecutionNotFound(execution_id))?;
        if row.status == "running" {
            return Err(EngineError::InvalidRequest(format!(
                "execution {execution_id} is mid-tick, retry shortly"
            )));
        }
        let checkpoint = PgCheckpointStore::new(self.db.clone())
            .latest(engine_core::ExecutionId(execution_id))
            .await
            .map_err(EngineError::InvalidRequest)?;
        let (state, current_nodes) = checkpoint.clone().map_or_else(
            || {
                (
                    serde_json::json!({}),
                    serde_json::from_value(row.current_nodes.clone()).unwrap_or_default(),
                )
            },
            |c| (c.state, c.current_nodes),
        );
        let execution = engine_core::Execution {
            id: engine_core::ExecutionId(execution_id),
            graph_id: engine_core::GraphId(row.graph_id.clone()),
            graph_version: u32::try_from(row.graph_version).unwrap_or(0),
            user_id: row.user_id.map(engine_core::UserId),
            status: wire::columns_to_status(&row.status, row.wait_kind.clone())
                .map_err(EngineError::InvalidRequest)?,
            current_nodes,
            state,
            iteration: u32::try_from(row.iteration).unwrap_or(0),
            max_iterations: u32::try_from(row.max_iterations).unwrap_or(0),
            deadline: row.deadline,
            budget: serde_json::from_value(row.budget.clone())?,
        };
        Ok((row, execution))
    }

    async fn persist(
        &self,
        execution_id: Uuid,
        next_step: u32,
        execution: &engine_core::Execution,
        events: &[engine_core::ExecutionEvent],
    ) -> Result<(), EngineError> {
        // Same shape as tick.rs's commit(): checkpoint, events, status/lease update and the
        // NOTIFY all land in one transaction, so a failure part-way can't leave a checkpoint
        // without its events (or a listener woken for a write that never committed).
        let checkpoint = Checkpoint {
            schema_version: CHECKPOINT_SCHEMA_VERSION,
            execution_id: engine_core::ExecutionId(execution_id),
            step: next_step,
            state: execution.state.clone(),
            current_nodes: execution.current_nodes.clone(),
        };
        let (status_column, wait_kind_column) = wire::status_to_columns(&execution.status);
        let current_nodes_json = serde_json::to_value(&execution.current_nodes)?;

        let txn = self.db.begin().await?;
        wire::checkpoint_to_active_model(&checkpoint)
            .insert(&txn)
            .await?;
        for event in events {
            wire::event_to_active_model(event).insert(&txn).await?;
        }
        crate::lease::release_lease(
            &txn,
            execution_id,
            &status_column,
            wait_kind_column,
            current_nodes_json,
        )
        .await?;
        txn.execute_unprepared("NOTIFY engine_tick").await?;
        txn.commit().await?;
        Ok(())
    }

    pub async fn interrupt(&self, execution_id: Uuid, input_json: &str) -> Result<(), EngineError> {
        let input: Value = serde_json::from_str(input_json)
            .map_err(|e| EngineError::InvalidRequest(e.to_string()))?;
        let (_, execution) = self.load(execution_id).await?;
        let next_step = PgCheckpointStore::new(self.db.clone())
            .latest(engine_core::ExecutionId(execution_id))
            .await
            .map_err(EngineError::InvalidRequest)?
            .map_or(0, |c| c.step + 1);
        let (execution, event) = engine_core::interrupt(execution, input);
        self.persist(
            execution_id,
            next_step,
            &execution,
            std::slice::from_ref(&event),
        )
        .await
    }

    pub async fn resume(
        &self,
        execution_id: Uuid,
        _wait_key: &str,
        event_json: &str,
    ) -> Result<(), EngineError> {
        // wait_key isn't checked against the execution's actual WaitKind yet — every Wait today
        // is UserInput (Interrupt covers it) or a Subgraph's ExternalEvent, which Task 15 already
        // documents as not wired at the service level; Resume exists so the proto surface is
        // complete for when Tool Service's Approval and a wired Subgraph both need it.
        let event: Value = serde_json::from_str(event_json)
            .map_err(|e| EngineError::InvalidRequest(e.to_string()))?;
        let (_, mut execution) = self.load(execution_id).await?;
        // Unlike interrupt()/cancel(), which legitimately accept a broader set of statuses,
        // Resume only means "the thing this execution was waiting for happened" — without this
        // guard it would happily revive a Completed/Failed/Cancelled execution back to Ready.
        if !matches!(execution.status, engine_core::Status::Waiting(_)) {
            return Err(EngineError::InvalidRequest(format!(
                "execution {execution_id} is not waiting (status: {:?})",
                execution.status
            )));
        }
        let next_step = PgCheckpointStore::new(self.db.clone())
            .latest(engine_core::ExecutionId(execution_id))
            .await
            .map_err(EngineError::InvalidRequest)?
            .map_or(0, |c| c.step + 1);
        if let Some(object) = execution.state.as_object_mut() {
            object.insert("resume_event".to_owned(), event);
        }
        execution.status = engine_core::Status::Ready;
        self.persist(execution_id, next_step, &execution, &[]).await
    }

    pub async fn cancel(&self, execution_id: Uuid) -> Result<(), EngineError> {
        let (_, mut execution) = self.load(execution_id).await?;
        let next_step = PgCheckpointStore::new(self.db.clone())
            .latest(engine_core::ExecutionId(execution_id))
            .await
            .map_err(EngineError::InvalidRequest)?
            .map_or(0, |c| c.step + 1);
        execution.status = engine_core::Status::Cancelled;
        let event = execution.event(engine_core::ExecutionPayload::ExecutionCancelled);
        self.persist(
            execution_id,
            next_step,
            &execution,
            std::slice::from_ref(&event),
        )
        .await
    }
}

#[cfg(all(test, feature = "test-support"))]
mod tests {
    use super::*;

    fn a_graph() -> String {
        serde_json::to_string(
            &engine_core::GraphBuilder::new()
                .entry("a")
                .task(
                    "a",
                    "noop",
                    serde_json::json!({"state_key": "reply", "reducer": "Replace"}),
                )
                .end("end")
                .edge("a", "end", engine_core::Condition::Always)
                .build("t", 0), // version in the builder's own output is ignored — register_graph assigns the real one
        )
        .expect("serialize")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn register_graph_assigns_version_one_then_increments() {
        let test = crate::test_db::start().await;
        let service = Service::new(test.db.clone());

        let (id, v1) = service
            .register_graph("t".to_owned(), &a_graph())
            .await
            .expect("register");
        assert_eq!((id.as_str(), v1), ("t", 1));

        let (_, v2) = service
            .register_graph("t".to_owned(), &a_graph())
            .await
            .expect("register again");
        assert_eq!(v2, 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn register_graph_rejects_an_invalid_graph() {
        let test = crate::test_db::start().await;
        let service = Service::new(test.db.clone());
        let bad =
            r#"{"id":"t","version":1,"user_id":null,"nodes":[],"edges":[],"entry":"missing"}"#;

        assert!(service.register_graph("t".to_owned(), bad).await.is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn start_execution_seeds_a_checkpoint_with_the_caller_input() {
        let test = crate::test_db::start().await;
        let service = Service::new(test.db.clone());
        service
            .register_graph("t".to_owned(), &a_graph())
            .await
            .expect("register");

        let execution_id = service
            .start_execution("t".to_owned(), None, r#"{"question": "hi"}"#, None)
            .await
            .expect("start");

        let execution = service.get_execution(execution_id).await.expect("get");
        assert_eq!(execution.state, serde_json::json!({"question": "hi"}));
        assert_eq!(
            execution.current_nodes,
            vec![ActiveNode::plain(engine_core::NodeId("a".into()))]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn start_execution_against_an_unregistered_graph_fails() {
        let test = crate::test_db::start().await;
        let service = Service::new(test.db.clone());

        let result = service
            .start_execution("nope".to_owned(), None, "{}", None)
            .await;
        assert!(matches!(result, Err(EngineError::GraphNotFound(..))));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn interrupt_wakes_a_waiting_execution() {
        let test = crate::test_db::start().await;
        let service = Service::new(test.db.clone());
        let waiting_graph = serde_json::to_string(
            &engine_core::GraphBuilder::new()
                .entry("ask")
                .task("ask", "noop", serde_json::json!({}))
                .wait("pause", engine_core::WaitKind::UserInput)
                .build("waiter", 0),
        )
        .expect("serialize");
        service
            .register_graph("waiter".to_owned(), &waiting_graph)
            .await
            .expect("register");
        // Seed directly in Waiting status — reaching it via a real tick is Task 23's job, this
        // test only exercises interrupt()'s own read/compute/persist path.
        let execution_id = Uuid::new_v4();
        entity::execution::ActiveModel {
            id: Set(execution_id),
            graph_id: Set("waiter".to_owned()),
            graph_version: Set(1),
            user_id: Set(None),
            status: Set("waiting".to_owned()),
            wait_kind: Set(Some(
                serde_json::to_value(engine_core::WaitKind::UserInput).unwrap(),
            )),
            current_nodes: Set(
                serde_json::to_value(vec![ActiveNode::plain(engine_core::NodeId("pause".into()))])
                    .unwrap(),
            ),
            iteration: Set(1),
            max_iterations: Set(10),
            deadline: Set(None),
            budget: Set(serde_json::to_value(Budget::new(
                1000,
                10,
                std::time::Duration::from_secs(60),
            ))
            .unwrap()),
            lease_owner: Set(None),
            lease_until: Set(None),
            created_at: Set(Utc::now()),
            updated_at: Set(Utc::now()),
        }
        .insert(&test.db)
        .await
        .expect("seed");

        service
            .interrupt(execution_id, r#"{"role": "user", "text": "hi"}"#)
            .await
            .expect("interrupt");

        let execution = service.get_execution(execution_id).await.expect("get");
        assert_eq!(execution.status, engine_core::Status::Ready);
        assert_eq!(
            execution.state.get("interrupt_input"),
            Some(&serde_json::json!({"role": "user", "text": "hi"}))
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn start_execution_rejects_input_json_that_is_not_an_object() {
        let test = crate::test_db::start().await;
        let service = Service::new(test.db.clone());
        service
            .register_graph("t".to_owned(), &a_graph())
            .await
            .expect("register");

        let result = service
            .start_execution("t".to_owned(), None, "5", None)
            .await;

        assert!(matches!(result, Err(EngineError::InvalidRequest(_))));
        let rows = entity::execution::Entity::find()
            .all(&test.db)
            .await
            .expect("query");
        assert!(rows.is_empty(), "no execution row was created");
        let checkpoints = entity::checkpoint::Entity::find()
            .all(&test.db)
            .await
            .expect("query");
        assert!(checkpoints.is_empty(), "no checkpoint row was created");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn resume_refuses_an_execution_that_is_not_waiting() {
        let test = crate::test_db::start().await;
        let service = Service::new(test.db.clone());
        service
            .register_graph("t".to_owned(), &a_graph())
            .await
            .expect("register");
        let execution_id = Uuid::new_v4();
        entity::execution::ActiveModel {
            id: Set(execution_id),
            graph_id: Set("t".to_owned()),
            graph_version: Set(1),
            user_id: Set(None),
            status: Set("completed".to_owned()),
            wait_kind: Set(None),
            current_nodes: Set(serde_json::json!([])),
            iteration: Set(3),
            max_iterations: Set(10),
            deadline: Set(None),
            budget: Set(serde_json::to_value(Budget::new(
                1000,
                10,
                std::time::Duration::from_secs(60),
            ))
            .unwrap()),
            lease_owner: Set(None),
            lease_until: Set(None),
            created_at: Set(Utc::now()),
            updated_at: Set(Utc::now()),
        }
        .insert(&test.db)
        .await
        .expect("seed");

        let result = service.resume(execution_id, "anything", "{}").await;

        assert!(matches!(result, Err(EngineError::InvalidRequest(_))));
        let row = entity::execution::Entity::find_by_id(execution_id)
            .one(&test.db)
            .await
            .expect("query")
            .expect("row");
        assert_eq!(
            row.status, "completed",
            "a finished execution is not revived"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancel_marks_the_execution_cancelled() {
        let test = crate::test_db::start().await;
        let service = Service::new(test.db.clone());
        service
            .register_graph("t".to_owned(), &a_graph())
            .await
            .expect("register");
        let execution_id = service
            .start_execution("t".to_owned(), None, "{}", None)
            .await
            .expect("start");

        service.cancel(execution_id).await.expect("cancel");

        let execution = service.get_execution(execution_id).await.expect("get");
        assert_eq!(execution.status, engine_core::Status::Cancelled);
    }
}
