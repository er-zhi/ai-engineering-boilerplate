// Integration tests for the tick loop, lease recovery and the sweep against Postgres.

use std::time::Duration;

use chrono::Utc;
use engine::error::EngineError;
use engine::lease::claim_one_ready;
use engine::service::Service;
use engine::sweep::sweep_terminal;
use engine::tick::Tick;
use engine_core::fakes::FakeTaskExecutor;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter,
    QueryOrder,
};
use serde_json::json;

async fn register_simple(service: &Service) {
    let definition = serde_json::to_string(&engine_core::simple_graph()).expect("serialize");
    service
        .register_graph("simple".to_owned(), &definition)
        .await
        .expect("register");
}

async fn register_timer_graph(service: &Service, until: chrono::DateTime<Utc>) {
    let definition = serde_json::to_string(
        &engine_core::GraphBuilder::new()
            .entry("work")
            .task("work", "noop", json!({}))
            .wait("pause", engine_core::WaitKind::Timer { until })
            .end("end")
            .edge("work", "pause", engine_core::Condition::Always)
            .edge("pause", "end", engine_core::Condition::Always)
            .build("timer", 1),
    )
    .expect("serialize");
    service
        .register_graph("timer".to_owned(), &definition)
        .await
        .expect("register");
}

async fn seed_waiting_on_timer(
    db: &sea_orm::DatabaseConnection,
    until: chrono::DateTime<Utc>,
) -> uuid::Uuid {
    let execution_id = uuid::Uuid::new_v4();
    engine::entity::execution::ActiveModel {
        id: Set(execution_id),
        graph_id: Set("timer".to_owned()),
        graph_version: Set(1),
        user_id: Set(None),
        status: Set(engine::entity::execution::Status::Waiting),
        wait_kind: Set(Some(
            serde_json::to_value(engine_core::WaitKind::Timer { until }).expect("serialize"),
        )),
        current_nodes: Set(serde_json::to_value(vec![engine_core::ActiveNode::plain(
            engine_core::NodeId("pause".into()),
        )])
        .expect("serialize")),
        iteration: Set(1),
        max_iterations: Set(10),
        deadline: Set(None),
        budget: Set(serde_json::to_value(engine_core::Budget::new(
            1000,
            10,
            Duration::from_secs(600),
        ))
        .expect("serialize")),
        lease_owner: Set(None),
        lease_until: Set(None),
        created_at: Set(Utc::now()),
        updated_at: Set(Utc::now()),
    }
    .insert(db)
    .await
    .expect("seed waiting execution");
    execution_id
}

#[tokio::test(flavor = "multi_thread")]
async fn a_linear_graph_completes_in_one_tick() {
    let test = engine::test_db::start().await;
    let service = Service::new(test.db.clone());
    register_simple(&service).await;
    let execution_id = service
        .start_execution("simple".to_owned(), None, r#"{"question": "hi"}"#, None)
        .await
        .expect("start");

    let executor = FakeTaskExecutor::default();
    executor.respond("llm", Ok(json!({"tool_call": null, "reply": "hello!"})));
    let tick = Tick::new(test.db.clone(), executor, "test-worker".to_owned());

    let did_work = tick.run_one().await.expect("tick");
    assert!(did_work);

    let execution = service.get_execution(execution_id).await.expect("get");
    assert_eq!(execution.status, engine_core::Status::Completed);
    assert_eq!(execution.state["llm"]["reply"], json!("hello!"));
}

#[tokio::test(flavor = "multi_thread")]
async fn nothing_ready_leaves_the_queue_untouched() {
    let test = engine::test_db::start().await;
    let executor = FakeTaskExecutor::default();
    let tick = Tick::new(test.db.clone(), executor, "test-worker".to_owned());

    assert!(!tick.run_one().await.expect("tick"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_crashed_workers_lease_is_reclaimed_and_the_execution_still_completes() {
    let test = engine::test_db::start().await;
    let service = Service::new(test.db.clone());
    register_simple(&service).await;
    let execution_id = service
        .start_execution("simple".to_owned(), None, r#"{"question": "hi"}"#, None)
        .await
        .expect("start");

    let claimed = claim_one_ready(
        &test.db,
        "worker-1-about-to-crash",
        Duration::from_millis(1),
    )
    .await
    .expect("claim")
    .expect("claimed");
    assert_eq!(claimed, execution_id);
    tokio::time::sleep(Duration::from_millis(20)).await;

    let executor = FakeTaskExecutor::default();
    executor.respond("llm", Ok(json!({"tool_call": null, "reply": "recovered"})));
    let tick = Tick::new(test.db.clone(), executor, "worker-2".to_owned());
    let did_work = tick.run_one().await.expect("tick");
    assert!(did_work);

    let execution = service.get_execution(execution_id).await.expect("get");
    assert_eq!(execution.status, engine_core::Status::Completed);
    assert_eq!(execution.state["llm"]["reply"], json!("recovered"));

    let row = engine::entity::execution::Entity::find_by_id(execution_id)
        .one(&test.db)
        .await
        .expect("query")
        .expect("row");
    assert_eq!(
        row.lease_owner, None,
        "a completed execution holds no lease"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_timer_whose_deadline_passed_is_reclaimed_and_the_execution_completes() {
    let test = engine::test_db::start().await;
    let service = Service::new(test.db.clone());
    let until = Utc::now() - chrono::Duration::seconds(1);
    register_timer_graph(&service, until).await;
    let execution_id = seed_waiting_on_timer(&test.db, until).await;

    let tick = Tick::new(
        test.db.clone(),
        FakeTaskExecutor::default(),
        "test-worker".to_owned(),
    );
    let did_work = tick.run_one().await.expect("tick");
    assert!(did_work, "an elapsed Timer is claimable work");

    let execution = service.get_execution(execution_id).await.expect("get");
    assert_eq!(execution.status, engine_core::Status::Completed);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_timer_that_has_not_elapsed_yet_is_left_alone() {
    let test = engine::test_db::start().await;
    let service = Service::new(test.db.clone());
    let until = Utc::now() + chrono::Duration::hours(1);
    register_timer_graph(&service, until).await;
    let execution_id = seed_waiting_on_timer(&test.db, until).await;

    let tick = Tick::new(
        test.db.clone(),
        FakeTaskExecutor::default(),
        "test-worker".to_owned(),
    );

    assert!(
        !tick.run_one().await.expect("tick"),
        "a Timer an hour out is not due — nothing to claim"
    );
    let execution = service.get_execution(execution_id).await.expect("get");
    assert_eq!(
        execution.status,
        engine_core::Status::Waiting(engine_core::WaitKind::Timer { until }),
        "the row is untouched, wait_kind included"
    );
}

async fn run_to_completion(test: &common::test_db::TestDb, service: &Service) -> uuid::Uuid {
    let execution_id = service
        .start_execution("simple".to_owned(), None, r#"{"question": "hi"}"#, None)
        .await
        .expect("start");
    let executor = FakeTaskExecutor::default();
    executor.respond("llm", Ok(json!({"tool_call": null, "reply": "done"})));
    let tick = Tick::new(test.db.clone(), executor, "sweeper-test".to_owned());
    assert!(tick.run_one().await.expect("tick"));
    execution_id
}

async fn checkpoint_count(db: &sea_orm::DatabaseConnection, execution_id: uuid::Uuid) -> u64 {
    engine::entity::checkpoint::Entity::find()
        .filter(engine::entity::checkpoint::Column::ExecutionId.eq(execution_id))
        .count(db)
        .await
        .expect("count checkpoints")
}

async fn event_payload_kinds(
    db: &sea_orm::DatabaseConnection,
    execution_id: uuid::Uuid,
) -> Vec<String> {
    engine::execution_event::Entity::find()
        .filter(engine::execution_event::Column::ExecutionId.eq(execution_id))
        .order_by_asc(engine::execution_event::Column::Id)
        .all(db)
        .await
        .expect("read events")
        .into_iter()
        .map(|row| {
            row.payload
                .as_object()
                .and_then(|object| object.keys().next().cloned())
                .unwrap_or_default()
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_sweep_takes_a_finished_execution_but_never_its_events() {
    let test = engine::test_db::start().await;
    let service = Service::new(test.db.clone());
    register_simple(&service).await;
    let execution_id = run_to_completion(&test, &service).await;
    assert!(
        checkpoint_count(&test.db, execution_id).await > 0,
        "it checkpointed while running"
    );

    let swept = sweep_terminal(&test.db, Duration::ZERO, 500)
        .await
        .expect("sweep");

    assert_eq!(swept, 1, "the one completed execution");
    assert!(
        engine::entity::execution::Entity::find_by_id(execution_id)
            .one(&test.db)
            .await
            .expect("query")
            .is_none(),
        "the hot row is gone"
    );
    assert_eq!(
        checkpoint_count(&test.db, execution_id).await,
        0,
        "and so are its checkpoints — they are a restart point, not history"
    );
    let kinds = event_payload_kinds(&test.db, execution_id).await;
    assert!(!kinds.is_empty(), "the event log is untouched: {kinds:?}");
    assert_eq!(
        kinds.last().map(String::as_str),
        Some("ExecutionCompleted"),
        "and it still ends with the final word on this execution: {kinds:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn get_execution_on_a_swept_id_is_not_found() {
    let test = engine::test_db::start().await;
    let service = Service::new(test.db.clone());
    register_simple(&service).await;
    let execution_id = run_to_completion(&test, &service).await;
    sweep_terminal(&test.db, Duration::ZERO, 500)
        .await
        .expect("sweep");

    let result = service.get_execution(execution_id).await;

    assert!(
        matches!(result, Err(EngineError::ExecutionNotFound(id)) if id == execution_id),
        "expected NotFound after the sweep, got {result:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_sweep_leaves_unfinished_work_and_fresh_results_alone() {
    let test = engine::test_db::start().await;
    let service = Service::new(test.db.clone());
    register_simple(&service).await;
    let completed = run_to_completion(&test, &service).await;
    let ready = service
        .start_execution("simple".to_owned(), None, r#"{"question": "later"}"#, None)
        .await
        .expect("start");

    let swept = sweep_terminal(&test.db, Duration::from_secs(3600), 500)
        .await
        .expect("sweep");

    assert_eq!(
        swept, 0,
        "a just-completed execution is still inside its grace period"
    );
    for (id, what) in [(completed, "the completed one"), (ready, "the ready one")] {
        assert!(
            engine::entity::execution::Entity::find_by_id(id)
                .one(&test.db)
                .await
                .expect("query")
                .is_some(),
            "{what} is still there"
        );
    }

    assert_eq!(
        sweep_terminal(&test.db, Duration::ZERO, 500)
            .await
            .expect("sweep"),
        1
    );
    assert!(
        engine::entity::execution::Entity::find_by_id(ready)
            .one(&test.db)
            .await
            .expect("query")
            .is_some(),
        "an execution that has not run yet is working set, whatever the retention says"
    );
}
