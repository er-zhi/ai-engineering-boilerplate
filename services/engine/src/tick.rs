// One unit of the event loop: claim an execution, run its nodes, commit the result.

use std::time::Duration;

use chrono::Utc;
use engine_core::{
    ActiveNode, CheckpointStore, Execution, Graph, Node, NodeOutput, TaskExecutor, step,
};
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};
use serde_json::{Value, json};

use crate::entity;
use crate::lease;
use crate::store::PgCheckpointStore;
use crate::wire;

const LEASE_DURATION: Duration = Duration::from_secs(300);

pub struct Tick<E: TaskExecutor> {
    db: DatabaseConnection,
    checkpoints: PgCheckpointStore,
    executor: std::sync::Arc<E>,
    owner: String,
}

impl<E: TaskExecutor + 'static> Tick<E> {
    #[must_use]
    pub fn new(db: DatabaseConnection, executor: E, owner: String) -> Self {
        Self {
            checkpoints: PgCheckpointStore::new(db.clone()),
            db,
            executor: std::sync::Arc::new(executor),
            owner,
        }
    }

    pub async fn run_one(&self) -> Result<bool, String> {
        let Some(execution_id) = lease::claim_one_ready(&self.db, &self.owner, LEASE_DURATION)
            .await
            .map_err(|e| e.to_string())?
        else {
            return Ok(false);
        };
        self.process(execution_id).await.map(|()| true)
    }

    async fn process(&self, execution_id: uuid::Uuid) -> Result<(), String> {
        let (row, graph) = self.load_execution_and_graph(execution_id).await?;
        let (execution, next_step) = self.load_current_state(execution_id, &row, &graph).await?;

        // Captured before `step` consumes `execution`: the node(s) this step's output came
        // from, offered to `commit_step` as where a queued follow-up can resume if this step
        // turns out to complete the execution — see `store::resolve_queued_input`.
        let resumable_nodes = execution.current_nodes.clone();
        let outputs = self
            .run_active_nodes(&graph, &execution, execution.current_nodes.clone())
            .await?;
        let (next_execution, events) = step(&graph, execution, outputs, Utc::now());
        crate::store::commit_step(
            &self.db,
            execution_id,
            Some(&self.owner),
            next_step,
            &next_execution,
            &events,
            Some(&resumable_nodes),
        )
        .await
        .map_err(|e| e.to_string())
    }

    async fn load_execution_and_graph(
        &self,
        execution_id: uuid::Uuid,
    ) -> Result<(entity::execution::Model, Graph), String> {
        let row = entity::execution::Entity::find_by_id(execution_id)
            .one(&self.db)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("claimed execution {execution_id} vanished"))?;
        let graph_row = entity::graph::Entity::find()
            .filter(entity::graph::Column::GraphId.eq(row.graph_id.clone()))
            .filter(entity::graph::Column::Version.eq(row.graph_version))
            .one(&self.db)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| {
                format!(
                    "graph {}v{} not registered",
                    row.graph_id, row.graph_version
                )
            })?;
        let graph: Graph =
            serde_json::from_value(graph_row.definition).map_err(|e| e.to_string())?;
        Ok((row, graph))
    }

    async fn load_current_state(
        &self,
        execution_id: uuid::Uuid,
        row: &entity::execution::Model,
        graph: &Graph,
    ) -> Result<(Execution, u32), String> {
        let checkpoint = self
            .checkpoints
            .latest(engine_core::ExecutionId(execution_id))
            .await?;
        let next_step = checkpoint.as_ref().map_or(0, |c| c.step + 1);
        let (state, current_nodes) = checkpoint.map_or_else(
            || {
                (
                    json!({}),
                    serde_json::from_value::<Vec<ActiveNode>>(row.current_nodes.clone())
                        .unwrap_or_default(),
                )
            },
            |c| (c.state, c.current_nodes),
        );

        if current_nodes
            .iter()
            .any(|active| matches!(graph.node(&active.node), Some(Node::Subgraph { .. })))
        {
            return Err("Subgraph is not wired at the service level yet".to_owned());
        }

        let status = wire::columns_to_status(row.status, row.wait_kind.clone())?;
        let execution = Execution {
            id: engine_core::ExecutionId(execution_id),
            graph_id: engine_core::GraphId(row.graph_id.clone()),
            graph_version: wire::counter_column(row.graph_version, "graph_version")?,
            user_id: row.user_id.map(engine_core::UserId),
            status,
            current_nodes,
            state,
            iteration: wire::counter_column(row.iteration, "iteration")?,
            max_iterations: wire::counter_column(row.max_iterations, "max_iterations")?,
            deadline: row.deadline,
            budget: serde_json::from_value(row.budget.clone()).map_err(|e| e.to_string())?,
        };
        Ok((execution, next_step))
    }

    async fn run_active_nodes(
        &self,
        graph: &Graph,
        execution: &Execution,
        current_nodes: Vec<ActiveNode>,
    ) -> Result<Vec<NodeOutput>, String> {
        let mut join_set = tokio::task::JoinSet::new();
        for active in current_nodes {
            let task_call = match graph.node(&active.node) {
                Some(Node::Task { kind, config, .. }) => Some((kind.clone(), config.clone())),
                _ => None,
            };
            let state_for_call = branch_state(&execution.state, graph, &active);
            let key = format!("{}:{}:{}", execution.id, active.node, execution.iteration);
            let executor = std::sync::Arc::clone(&self.executor);
            join_set.spawn(async move {
                let result = match task_call {
                    Some((kind, config)) => executor
                        .execute(&kind, &config, &state_for_call, &key)
                        .await
                        .map_err(|e| e.to_string()),
                    None => Ok(Value::Null),
                };
                NodeOutput {
                    node: active,
                    result,
                }
            });
        }
        let mut outputs = Vec::new();
        while let Some(joined) = join_set.join_next().await {
            outputs.push(joined.map_err(|e| e.to_string())?);
        }
        Ok(outputs)
    }
}

fn branch_state(state: &Value, graph: &Graph, active: &ActiveNode) -> Value {
    let Some(branch) = active.branch else {
        return state.clone();
    };
    let Some(Node::FanOut {
        source, item_var, ..
    }) = graph
        .nodes
        .iter()
        .find(|node| matches!(node, Node::FanOut { target, .. } if target == &active.node))
    else {
        return state.clone();
    };
    let mut scoped = state.clone();
    if let Some(item) = state
        .pointer(source)
        .and_then(Value::as_array)
        .and_then(|items| items.get(branch as usize))
        && let Some(object) = scoped.as_object_mut()
    {
        object.insert(item_var.clone(), item.clone());
    }
    scoped
}

#[cfg(all(test, feature = "test-support"))]
mod tests {
    use super::*;
    use engine_core::fakes::FakeTaskExecutor;
    use sea_orm::{ActiveModelTrait, ActiveValue::Set};

    async fn seed_graph(db: &DatabaseConnection) {
        crate::entity::graph::ActiveModel {
            graph_id: Set("t".to_owned()),
            version: Set(1),
            user_id: Set(None),
            definition: Set(serde_json::to_value(
                engine_core::GraphBuilder::new()
                    .entry("a")
                    .task(
                        "a",
                        "noop",
                        json!({"state_key": "reply", "reducer": "Replace"}),
                    )
                    .end("end")
                    .edge("a", "end", engine_core::Condition::Always)
                    .build("t", 1),
            )
            .expect("serialize")),
            created_at: Set(Utc::now()),
            ..Default::default()
        }
        .insert(db)
        .await
        .expect("seed graph");
    }

    async fn seed_execution(db: &DatabaseConnection) -> uuid::Uuid {
        let id = uuid::Uuid::new_v4();
        crate::entity::execution::ActiveModel {
            id: Set(id),
            graph_id: Set("t".to_owned()),
            graph_version: Set(1),
            user_id: Set(None),
            status: Set(crate::entity::execution::Status::Ready),
            wait_kind: Set(None),
            current_nodes: Set(
                serde_json::to_value(vec![ActiveNode::plain(engine_core::NodeId("a".into()))])
                    .expect("serialize"),
            ),
            iteration: Set(0),
            max_iterations: Set(10),
            deadline: Set(None),
            budget: Set(serde_json::to_value(engine_core::Budget::new(
                1000,
                10,
                Duration::from_secs(60),
            ))
            .expect("serialize")),
            lease_owner: Set(None),
            lease_until: Set(None),
            created_at: Set(Utc::now()),
            updated_at: Set(Utc::now()),
        }
        .insert(db)
        .await
        .expect("seed execution");
        id
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_full_tick_runs_a_task_and_completes_the_execution() {
        let test = crate::test_db::start().await;
        seed_graph(&test.db).await;
        let execution_id = seed_execution(&test.db).await;

        let executor = FakeTaskExecutor::default();
        executor.respond("noop", Ok(json!({"text": "hi"})));
        let tick = Tick::new(test.db.clone(), executor, "worker-1".to_owned());

        let did_work = tick.run_one().await.expect("tick");
        assert!(did_work);

        let row = crate::entity::execution::Entity::find_by_id(execution_id)
            .one(&test.db)
            .await
            .expect("query")
            .expect("row");
        assert_eq!(row.status, crate::entity::execution::Status::Completed);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_tick_with_nothing_ready_returns_false() {
        let test = crate::test_db::start().await;
        let executor = FakeTaskExecutor::default();
        let tick = Tick::new(test.db.clone(), executor, "worker-1".to_owned());

        assert!(!tick.run_one().await.expect("tick"));
    }
}
