// Registers graphs, starts executions and reads them back.

use chrono::Utc;
use engine_core::{
    ActiveNode, Budget, CHECKPOINT_SCHEMA_VERSION, Checkpoint, CheckpointStore, Graph,
};
use engine_core::{
    PRIOR_MATERIAL_STATE_KEY, RENDERED_TOOL_RESULTS, TOOL_RESULT_STATE_KEY, ToolRecord,
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

const DEFAULT_MAX_ITERATIONS: i16 = 25;
const DEFAULT_TOKEN_BUDGET: u32 = 200_000;
const DEFAULT_TOOL_CALL_BUDGET: u32 = 50;
const DEFAULT_WALL_TIME_BUDGET: std::time::Duration = std::time::Duration::from_secs(600);

fn counter_column(value: impl TryInto<u32>, column: &str) -> Result<u32, EngineError> {
    wire::counter_column(value, column).map_err(EngineError::Storage)
}

pub(crate) fn to_execution(
    row: &entity::execution::Model,
    checkpoint: Option<Checkpoint>,
) -> Result<engine_core::Execution, EngineError> {
    let (state, current_nodes) = checkpoint.map_or_else(
        || {
            (
                serde_json::json!({}),
                serde_json::from_value(row.current_nodes.clone()).unwrap_or_default(),
            )
        },
        |checkpoint| (checkpoint.state, checkpoint.current_nodes),
    );
    Ok(engine_core::Execution {
        id: engine_core::ExecutionId(row.id),
        graph_id: engine_core::GraphId(row.graph_id.clone()),
        graph_version: counter_column(row.graph_version, "graph_version")?,
        user_id: row.user_id.map(engine_core::UserId),
        status: wire::columns_to_status(row.status, row.wait_kind.clone())
            .map_err(EngineError::Storage)?,
        current_nodes,
        state,
        iteration: counter_column(row.iteration, "iteration")?,
        max_iterations: counter_column(row.max_iterations, "max_iterations")?,
        deadline: row.deadline,
        budget: serde_json::from_value(row.budget.clone())?,
    })
}

fn validated_graph(definition_json: &str) -> Result<Graph, EngineError> {
    let definition: Graph = serde_json::from_str(definition_json)
        .map_err(|error| EngineError::InvalidGraph(error.to_string()))?;
    definition
        .validate()
        .map_err(|problems| EngineError::InvalidGraph(problems.join("; ")))?;
    Ok(definition)
}

impl Service {
    async fn execution_belongs_to(&self, execution_id: Uuid, caller: Option<Uuid>) -> bool {
        match self.execution_row(execution_id).await {
            Ok(row) => row.user_id.is_none() || row.user_id == caller,
            Err(error) => {
                tracing::warn!(%execution_id, %error, "could not read the earlier turn's owner");
                false
            }
        }
    }
}

fn carried_records(state: &Value) -> Vec<Value> {
    let of = |key: &str| {
        state
            .get(key)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    let mut records = of(PRIOR_MATERIAL_STATE_KEY);
    records.extend(of(TOOL_RESULT_STATE_KEY));
    prefer_the_record_that_carries_the_call(&mut records, state);
    records.retain(reached_its_source);
    let oldest_beyond_what_a_follow_up_inherits =
        records.len().saturating_sub(RENDERED_TOOL_RESULTS);
    records.split_off(oldest_beyond_what_a_follow_up_inherits)
}

fn prefer_the_record_that_carries_the_call(records: &mut Vec<Value>, state: &Value) {
    let Some(with_the_call) = state
        .get(engine_core::LLM_STATE_KEY)
        .and_then(|llm| llm.get(engine_core::FAST_TOOL_CALL_FIELD))
    else {
        return;
    };
    let Some(full) = ToolRecord::read(with_the_call) else {
        return;
    };
    let as_json = full.to_json();
    let same_fetch = |record: &&mut Value| match ToolRecord::read(record) {
        Some(typed) => typed.result == full.result,
        None => **record == full.result,
    };
    match records.iter_mut().find(same_fetch) {
        Some(written_without_the_call) => *written_without_the_call = as_json,
        None => records.push(as_json),
    }
}

fn reached_its_source(record: &Value) -> bool {
    ToolRecord::read(record).is_none_or(|record| record.reached_its_source())
}

fn execution_input(input_json: &str) -> Result<Value, EngineError> {
    let input: Value = serde_json::from_str(input_json)
        .map_err(|error| EngineError::InvalidRequest(error.to_string()))?;
    if !input.is_object() {
        return Err(EngineError::InvalidRequest(
            "input_json must decode to a JSON object".to_owned(),
        ));
    }
    Ok(input)
}

fn already_stored(row: &entity::graph::Model, definition: &Graph) -> bool {
    let mut candidate = definition.clone();
    candidate.version = u32::try_from(row.version).unwrap_or(u32::MAX);
    serde_json::to_value(&candidate).is_ok_and(|json| json == row.definition)
}

pub struct Service {
    pub(crate) db: DatabaseConnection,
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
        let definition = validated_graph(definition_json)?;
        let newest = self.newest_graph_row(&graph_id).await?;
        self.insert_next_version(graph_id, definition, newest.as_ref())
            .await
    }

    pub async fn register_graph_if_changed(
        &self,
        graph_id: String,
        definition_json: &str,
    ) -> Result<(String, i32), EngineError> {
        let definition = validated_graph(definition_json)?;
        let newest = self.newest_graph_row(&graph_id).await?;
        if let Some(row) = &newest
            && already_stored(row, &definition)
        {
            return Ok((graph_id, row.version));
        }
        self.insert_next_version(graph_id, definition, newest.as_ref())
            .await
    }

    async fn newest_graph_row(
        &self,
        graph_id: &str,
    ) -> Result<Option<entity::graph::Model>, EngineError> {
        Ok(entity::graph::Entity::find()
            .filter(entity::graph::Column::GraphId.eq(graph_id))
            .order_by_desc(entity::graph::Column::Version)
            .one(&self.db)
            .await?)
    }

    async fn insert_next_version(
        &self,
        graph_id: String,
        mut definition: Graph,
        newest: Option<&entity::graph::Model>,
    ) -> Result<(String, i32), EngineError> {
        let next_version = newest.map_or(1, |row| row.version + 1);
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

    async fn load_graph(
        &self,
        graph_id: &str,
        version: Option<i32>,
    ) -> Result<(entity::graph::Model, Graph), EngineError> {
        let mut query =
            entity::graph::Entity::find().filter(entity::graph::Column::GraphId.eq(graph_id));
        query = match version {
            Some(v) => query.filter(entity::graph::Column::Version.eq(v)),
            None => query.order_by_desc(entity::graph::Column::Version),
        };
        let row = query
            .one(&self.db)
            .await?
            .ok_or_else(|| EngineError::GraphNotFound(graph_id.to_owned(), version))?;
        let definition: Graph = serde_json::from_value(row.definition.clone())?;
        Ok((row, definition))
    }

    pub(crate) async fn next_checkpoint_step(
        &self,
        execution_id: Uuid,
    ) -> Result<u32, EngineError> {
        Ok(PgCheckpointStore::new(self.db.clone())
            .latest(engine_core::ExecutionId(execution_id))
            .await
            .map_err(EngineError::Storage)?
            .map_or(0, |checkpoint| checkpoint.step + 1))
    }

    pub async fn start_execution(
        &self,
        graph_id: String,
        version: Option<i32>,
        input_json: &str,
        user_id: Option<Uuid>,
    ) -> Result<Uuid, EngineError> {
        self.start_continuing(graph_id, version, input_json, user_id, None)
            .await
    }

    pub async fn start_continuing(
        &self,
        graph_id: String,
        version: Option<i32>,
        input_json: &str,
        user_id: Option<Uuid>,
        continues: Option<Uuid>,
    ) -> Result<Uuid, EngineError> {
        let mut input = execution_input(input_json)?;
        if let Some(parent) = continues {
            self.carry_material_from(parent, user_id, &mut input).await;
        }
        let (graph_row, graph) = self.load_graph(&graph_id, version).await?;

        let execution_id = Uuid::new_v4();
        let budget = Budget::new(
            DEFAULT_TOKEN_BUDGET,
            DEFAULT_TOOL_CALL_BUDGET,
            DEFAULT_WALL_TIME_BUDGET,
        );
        let entry_nodes = vec![ActiveNode::plain(graph.entry.clone())];

        let txn = self.db.begin().await?;
        entity::execution::ActiveModel {
            id: Set(execution_id),
            graph_id: Set(graph_row.graph_id),
            graph_version: Set(graph_row.version),
            user_id: Set(user_id),
            status: Set(entity::execution::Status::Ready),
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
        .insert(&txn)
        .await?;
        wire::checkpoint_to_active_model(&Checkpoint {
            schema_version: CHECKPOINT_SCHEMA_VERSION,
            execution_id: engine_core::ExecutionId(execution_id),
            step: 0,
            state: input,
            current_nodes: entry_nodes,
        })
        .insert(&txn)
        .await?;
        txn.execute_unprepared(crate::store::WAKE_THE_NEXT_TICK)
            .await?;
        txn.commit().await?;
        Ok(execution_id)
    }

    async fn carry_material_from(&self, parent: Uuid, caller: Option<Uuid>, input: &mut Value) {
        if !self.execution_belongs_to(parent, caller).await {
            tracing::warn!(%parent, "earlier turn belongs to another user, carrying nothing");
            return;
        }
        let carried = match self.latest_checkpoint(parent).await {
            Ok(Some(checkpoint)) => Some(carried_records(&checkpoint.state)),
            Ok(None) => None,
            Err(error) => {
                tracing::warn!(%parent, %error, "could not read what the earlier turn found");
                None
            }
        };
        let Some(records) = carried.filter(|records| !records.is_empty()) else {
            return;
        };
        if let Some(object) = input.as_object_mut() {
            object.insert(
                PRIOR_MATERIAL_STATE_KEY.to_owned(),
                serde_json::Value::Array(records),
            );
        }
    }

    pub async fn get_execution(
        &self,
        execution_id: Uuid,
    ) -> Result<engine_core::Execution, EngineError> {
        let row = self.execution_row(execution_id).await?;
        let checkpoint = self.latest_checkpoint(execution_id).await?;
        to_execution(&row, checkpoint)
    }

    pub(crate) async fn execution_row(
        &self,
        execution_id: Uuid,
    ) -> Result<entity::execution::Model, EngineError> {
        entity::execution::Entity::find_by_id(execution_id)
            .one(&self.db)
            .await?
            .ok_or(EngineError::ExecutionNotFound(execution_id))
    }

    pub(crate) async fn latest_checkpoint(
        &self,
        execution_id: Uuid,
    ) -> Result<Option<Checkpoint>, EngineError> {
        PgCheckpointStore::new(self.db.clone())
            .latest(engine_core::ExecutionId(execution_id))
            .await
            .map_err(EngineError::Storage)
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
                .build("t", 0),
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
    async fn an_execution_whose_checkpoint_cannot_be_saved_is_never_left_claimable() {
        let test = crate::test_db::start().await;
        let service = Service::new(test.db.clone());
        service
            .register_graph("t".to_owned(), &a_graph())
            .await
            .expect("register");
        test.db
            .execute_unprepared("DROP TABLE engine.checkpoints")
            .await
            .expect("drop the table the step-0 checkpoint needs");

        let result = service
            .start_execution("t".to_owned(), None, r#"{"question": "hi"}"#, None)
            .await;

        assert!(result.is_err());
        let rows = entity::execution::Entity::find()
            .all(&test.db)
            .await
            .expect("query");
        assert!(
            rows.is_empty(),
            "the row and the checkpoint holding the caller's input commit together or not at all"
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
}
