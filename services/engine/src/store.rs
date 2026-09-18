// Persists checkpoints and execution events.

use engine_core::{
    ActiveNode, Checkpoint, CheckpointStore, Execution, ExecutionEvent, ExecutionId, Status,
};
use sea_orm::ActiveValue::Set;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseConnection, DbErr, EntityTrait,
    QueryFilter, QueryOrder, TransactionTrait,
};
use serde_json::Value;

use crate::entity::{checkpoint, pending_input};
use crate::lease;
use crate::wire::{
    checkpoint_from_model, checkpoint_to_active_model, event_to_active_model, status_to_columns,
};

pub const WAKE_THE_NEXT_TICK: &str = "NOTIFY engine_tick";

const STORAGE_FAILURE: &str = "engine could not reach its own storage";

const LEASE_LOST: &str = "another worker holds this execution's lease";

fn storage_failure(operation: &str, error: &sea_orm::DbErr) -> String {
    tracing::error!(%error, operation, "engine storage call failed");
    STORAGE_FAILURE.to_owned()
}

#[derive(Clone)]
pub struct PgCheckpointStore {
    db: DatabaseConnection,
}

impl PgCheckpointStore {
    #[must_use]
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }
}

impl CheckpointStore for PgCheckpointStore {
    async fn save(&self, checkpoint: &Checkpoint) -> Result<(), String> {
        checkpoint_to_active_model(checkpoint)
            .insert(&self.db)
            .await
            .map(|_| ())
            .map_err(|e| storage_failure("save checkpoint", &e))
    }

    async fn latest(&self, execution_id: ExecutionId) -> Result<Option<Checkpoint>, String> {
        let model = checkpoint::Entity::find()
            .filter(checkpoint::Column::ExecutionId.eq(execution_id.0))
            .order_by_desc(checkpoint::Column::Step)
            .one(&self.db)
            .await
            .map_err(|e| storage_failure("read latest checkpoint", &e))?;
        model.map(checkpoint_from_model).transpose()
    }
}

pub async fn commit_step(
    db: &DatabaseConnection,
    execution_id: uuid::Uuid,
    owner: Option<&str>,
    step: u32,
    execution: &Execution,
    events: &[ExecutionEvent],
    resumable_nodes: Option<&[ActiveNode]>,
) -> Result<(), DbErr> {
    let txn = db.begin().await?;

    // Whatever `control::interrupt` queued while this execution was mid-tick (see
    // `queue_pending_input`) lands here, at the one place every write to this execution's state
    // passes through — the tick loop's own step commit, and a direct `interrupt`/`resume`/
    // `cancel` once the execution is idle. `resolve_queued_input` applies it if there is still
    // an execution to apply it to, and discards it — never silently, always by deleting the row
    // — otherwise.
    let (execution, extra_events) =
        resolve_queued_input(&txn, execution_id, execution, resumable_nodes).await?;

    let checkpoint = Checkpoint {
        schema_version: engine_core::CHECKPOINT_SCHEMA_VERSION,
        execution_id: ExecutionId(execution_id),
        step,
        state: execution.state.clone(),
        current_nodes: execution.current_nodes.clone(),
    };
    let (status_column, wait_kind_column) = status_to_columns(&execution.status);
    let current_nodes_json = serde_json::to_value(&execution.current_nodes).unwrap_or_else(|_| {
        debug_assert!(false, "an ActiveNode list always serializes");
        serde_json::Value::Array(Vec::new())
    });

    checkpoint_to_active_model(&checkpoint).insert(&txn).await?;
    for event in events.iter().chain(extra_events.iter()) {
        event_to_active_model(event).insert(&txn).await?;
    }
    let released = lease::release_lease(
        &txn,
        execution_id,
        owner,
        status_column,
        wait_kind_column,
        current_nodes_json,
    )
    .await?;
    if released == 0 {
        txn.rollback().await?;
        return Err(DbErr::Custom(LEASE_LOST.to_owned()));
    }
    txn.execute_unprepared(WAKE_THE_NEXT_TICK).await?;
    txn.commit().await
}

/// Queues `input` for `execution_id` instead of applying it: called only while the execution is
/// `Running` (`control::Service::interrupt`), when nothing may write to its state without
/// racing the tick loop that owns it. `commit_step` drains this queue at the execution's next
/// boundary.
pub async fn queue_pending_input(
    db: &impl ConnectionTrait,
    execution_id: uuid::Uuid,
    input: &Value,
) -> Result<(), DbErr> {
    pending_input::ActiveModel {
        execution_id: Set(execution_id),
        input_json: Set(input.clone()),
        created_at: Set(chrono::Utc::now()),
        ..Default::default()
    }
    .insert(db)
    .await
    .map(|_| ())
}

/// Drains and applies whatever is queued for `execution_id`, oldest first, folding each entry
/// through `engine_core::interrupt` in turn so two inputs queued during one tick both land, in
/// the order they arrived. A row is always deleted once read, whether or not it goes on to be
/// applied.
///
/// A `Failed` or `Cancelled` outcome is a hard stop: whatever was queued for it is discarded,
/// not applied — reviving a budget failure or an explicit cancel because a message happened to
/// be in flight would be a surprise nobody asked for.
///
/// A `Completed` outcome needs more care. Several of this crate's built-in graphs (`agent`
/// among them) have no `Wait` node at all, so a question that needs no tool answers in a single
/// tick: the commit that ends the execution is the *only* boundary that ever occurs, with
/// nothing in between it and the `Running` row the caller's `interrupt` queued against.
/// Discarding here unconditionally would make that queueing a silent no-op for exactly the
/// turns most likely to still be running when a fast follow-up arrives — accepting the message
/// and then losing it, which is worse than refusing it outright. So when the caller can supply
/// `resumable_nodes` — the node(s) that were active going into the step that just produced
/// `Completed`, which only the tick loop has — a `Completed` outcome with something still
/// queued is treated as provisional: those nodes get the new input and one more turn instead of
/// ending the conversation under it. `resumable_nodes` is `None` for `control::Service::persist`
/// (`interrupt`'s idle path, `resume`, `cancel`), none of which can themselves produce
/// `Completed`, so this only ever fires from the tick loop.
async fn resolve_queued_input(
    txn: &impl ConnectionTrait,
    execution_id: uuid::Uuid,
    execution: &Execution,
    resumable_nodes: Option<&[ActiveNode]>,
) -> Result<(Execution, Vec<ExecutionEvent>), DbErr> {
    let queued = pending_input::Entity::find()
        .filter(pending_input::Column::ExecutionId.eq(execution_id))
        .order_by_asc(pending_input::Column::Id)
        .all(txn)
        .await?;
    if queued.is_empty() {
        return Ok((execution.clone(), Vec::new()));
    }
    pending_input::Entity::delete_many()
        .filter(pending_input::Column::ExecutionId.eq(execution_id))
        .exec(txn)
        .await?;

    let mut current = execution.clone();
    match (&current.status, resumable_nodes) {
        (Status::Failed | Status::Cancelled, _) | (Status::Completed, None) => {
            return Ok((current, Vec::new()));
        }
        (Status::Completed, Some(nodes)) => current.current_nodes = nodes.to_vec(),
        // `step` and `engine_core::interrupt` never themselves produce `Running` — only
        // `lease::claim_one_ready` sets it, on the row, when claiming — so this arm is
        // unreached in practice. Matched explicitly, and treated like `Ready`, only because
        // `Status` gives no other way to write an exhaustive match.
        (Status::Ready | Status::Waiting(_) | Status::Running, _) => {}
    }

    let mut events = Vec::with_capacity(queued.len());
    for row in queued {
        let (next, event) = engine_core::interrupt(current, row.input_json);
        current = next;
        events.push(event);
    }
    Ok((current, events))
}

#[cfg(all(test, feature = "test-support"))]
mod tests {
    use super::*;
    use crate::entity::execution;
    use engine_core::{ActiveNode, NodeId};

    #[tokio::test(flavor = "multi_thread")]
    async fn checkpoint_store_saves_and_reads_back_the_latest() {
        let test = crate::test_db::start().await;
        let store = PgCheckpointStore::new(test.db.clone());
        let execution_id = ExecutionId(uuid::Uuid::new_v4());
        let make = |step| Checkpoint {
            schema_version: engine_core::CHECKPOINT_SCHEMA_VERSION,
            execution_id,
            step,
            state: serde_json::json!({"step": step}),
            current_nodes: vec![ActiveNode::plain(NodeId("llm".into()))],
        };
        store.save(&make(0)).await.expect("save 0");
        store.save(&make(1)).await.expect("save 1");

        let latest = store
            .latest(execution_id)
            .await
            .expect("latest")
            .expect("some");
        assert_eq!(latest.step, 1);
    }

    /// Seeds a bare `Ready`, unleased execution row: `commit_step` releases a lease by matching
    /// `execution_id` and `owner`, so a row has to exist for it to have anything to update.
    async fn seed_execution(db: &DatabaseConnection) -> uuid::Uuid {
        let id = uuid::Uuid::new_v4();
        execution::ActiveModel {
            id: Set(id),
            graph_id: Set("t".to_owned()),
            graph_version: Set(1),
            user_id: Set(None),
            status: Set(execution::Status::Running),
            wait_kind: Set(None),
            current_nodes: Set(serde_json::json!([])),
            iteration: Set(0),
            max_iterations: Set(10),
            deadline: Set(None),
            budget: Set(serde_json::to_value(engine_core::Budget::new(
                1000,
                10,
                std::time::Duration::from_secs(60),
            ))
            .expect("serialize")),
            lease_owner: Set(None),
            lease_until: Set(None),
            created_at: Set(chrono::Utc::now()),
            updated_at: Set(chrono::Utc::now()),
        }
        .insert(db)
        .await
        .expect("seed execution");
        id
    }

    /// The tick loop's own step output right before it commits: still going (`Ready`), unless
    /// the caller overrides `status` to model the tick that finishes the execution instead.
    fn a_step_result(execution_id: uuid::Uuid, state: serde_json::Value) -> Execution {
        Execution {
            id: ExecutionId(execution_id),
            graph_id: engine_core::GraphId("t".to_owned()),
            graph_version: 1,
            user_id: None,
            status: engine_core::Status::Ready,
            current_nodes: vec![ActiveNode::plain(NodeId("llm".into()))],
            state,
            iteration: 1,
            max_iterations: 10,
            deadline: None,
            budget: engine_core::Budget::new(1000, 10, std::time::Duration::from_secs(60)),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn commit_step_applies_input_queued_while_the_execution_was_mid_tick() {
        let test = crate::test_db::start().await;
        let execution_id = seed_execution(&test.db).await;
        queue_pending_input(
            &test.db,
            execution_id,
            &serde_json::json!({"question": "and the population?"}),
        )
        .await
        .expect("queue");

        let step_result = a_step_result(
            execution_id,
            serde_json::json!({"question": "what is the capital of France?"}),
        );
        commit_step(&test.db, execution_id, None, 0, &step_result, &[], None)
            .await
            .expect("commit");

        let checkpoint = PgCheckpointStore::new(test.db.clone())
            .latest(ExecutionId(execution_id))
            .await
            .expect("latest")
            .expect("a checkpoint was written");
        assert_eq!(
            checkpoint.state.get("question"),
            Some(&serde_json::json!("and the population?")),
            "a follow-up accepted while mid-tick must reach the field the llm node rereads, \
             not just be recorded somewhere nothing looks"
        );

        let remaining = pending_input::Entity::find()
            .filter(pending_input::Column::ExecutionId.eq(execution_id))
            .all(&test.db)
            .await
            .expect("query");
        assert!(
            remaining.is_empty(),
            "an applied entry must not linger in the queue"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn two_inputs_queued_during_one_turn_both_apply_in_order() {
        let test = crate::test_db::start().await;
        let execution_id = seed_execution(&test.db).await;
        queue_pending_input(
            &test.db,
            execution_id,
            &serde_json::json!({"question": "and the population?"}),
        )
        .await
        .expect("queue first");
        queue_pending_input(
            &test.db,
            execution_id,
            &serde_json::json!({"question": "and the area?"}),
        )
        .await
        .expect("queue second");

        let step_result = a_step_result(
            execution_id,
            serde_json::json!({"question": "what is the capital of France?"}),
        );
        commit_step(&test.db, execution_id, None, 0, &step_result, &[], None)
            .await
            .expect("commit");

        let checkpoint = PgCheckpointStore::new(test.db.clone())
            .latest(ExecutionId(execution_id))
            .await
            .expect("latest")
            .expect("a checkpoint was written");
        assert_eq!(
            checkpoint.state.get("question"),
            Some(&serde_json::json!("and the area?")),
            "applied oldest first, so the later follow-up is the one left standing"
        );

        let events = crate::execution_event::Entity::find()
            .filter(crate::execution_event::Column::ExecutionId.eq(execution_id))
            .order_by_asc(crate::execution_event::Column::Id)
            .all(&test.db)
            .await
            .expect("query events");
        assert_eq!(
            events.len(),
            2,
            "both queued follow-ups must land, not just the one that ends up in state"
        );
        assert!(
            events
                .iter()
                .all(|event| event.payload == serde_json::json!("Interrupted")),
            "{events:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_pending_input_does_not_survive_a_finishing_commit_with_no_resumable_nodes() {
        // `resumable_nodes: None` is what `control::Service::persist` always passes (`resume`
        // and `cancel` can only reach `Waiting`/`Ready`/`Cancelled`, never `Completed`, so this
        // exercises the belt-and-suspenders case, not the tick loop's own path — that path is
        // `a_completed_tick_with_a_queued_follow_up_gets_one_more_turn_instead_of_ending` below.
        let test = crate::test_db::start().await;
        let execution_id = seed_execution(&test.db).await;
        queue_pending_input(
            &test.db,
            execution_id,
            &serde_json::json!({"question": "too late"}),
        )
        .await
        .expect("queue");

        let mut step_result = a_step_result(
            execution_id,
            serde_json::json!({"question": "what is the capital of France?", "llm": {"reply": "Paris"}}),
        );
        step_result.status = engine_core::Status::Completed;
        step_result.current_nodes = Vec::new();
        commit_step(&test.db, execution_id, None, 0, &step_result, &[], None)
            .await
            .expect("commit");

        let remaining = pending_input::Entity::find()
            .filter(pending_input::Column::ExecutionId.eq(execution_id))
            .all(&test.db)
            .await
            .expect("query");
        assert!(
            remaining.is_empty(),
            "a queued follow-up must not outlive the execution it was meant for"
        );

        let row = execution::Entity::find_by_id(execution_id)
            .one(&test.db)
            .await
            .expect("query")
            .expect("row");
        assert_eq!(
            row.status,
            execution::Status::Completed,
            "discarding the queued input must not resurrect a finished execution"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_completed_tick_with_a_queued_follow_up_gets_one_more_turn_instead_of_ending() {
        // `agent` (and any tool-less turn through it) has no `Wait` node, so a question that
        // needs no tool completes in exactly one tick: the commit that ends it is the only
        // boundary that ever occurs. Discarding here unconditionally would make queueing a
        // silent no-op for precisely this — very common — case. The tick loop supplies
        // `resumable_nodes`, so this must come back `Ready` on those nodes with the follow-up
        // applied, not `Completed` with the follow-up dropped.
        let test = crate::test_db::start().await;
        let execution_id = seed_execution(&test.db).await;
        queue_pending_input(
            &test.db,
            execution_id,
            &serde_json::json!({"question": "and the population?"}),
        )
        .await
        .expect("queue");

        let mut step_result = a_step_result(
            execution_id,
            serde_json::json!({"question": "what is the capital of France?", "llm": {"reply": "Paris"}}),
        );
        step_result.status = engine_core::Status::Completed;
        step_result.current_nodes = Vec::new();
        let resumable_nodes = vec![ActiveNode::plain(NodeId("llm".into()))];
        commit_step(
            &test.db,
            execution_id,
            None,
            0,
            &step_result,
            &[],
            Some(&resumable_nodes),
        )
        .await
        .expect("commit");

        let checkpoint = PgCheckpointStore::new(test.db.clone())
            .latest(ExecutionId(execution_id))
            .await
            .expect("latest")
            .expect("a checkpoint was written");
        assert_eq!(
            checkpoint.state.get("question"),
            Some(&serde_json::json!("and the population?")),
            "the follow-up must actually reach the field the llm node rereads"
        );
        assert_eq!(
            checkpoint.current_nodes, resumable_nodes,
            "the node(s) that just answered get the follow-up, not the graph's empty end state"
        );

        let row = execution::Entity::find_by_id(execution_id)
            .one(&test.db)
            .await
            .expect("query")
            .expect("row");
        assert_eq!(
            row.status,
            execution::Status::Ready,
            "a follow-up that arrived in time must reopen the turn, not leave it Completed"
        );

        let remaining = pending_input::Entity::find()
            .filter(pending_input::Column::ExecutionId.eq(execution_id))
            .all(&test.db)
            .await
            .expect("query");
        assert!(remaining.is_empty(), "an applied entry must not linger");
    }
}
