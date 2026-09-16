// Integration tests: the tick loop against a real (testcontainers) Postgres, a fake
// TaskExecutor. Proves what a unit test can't — that claim/run/step/commit actually compose
// correctly against the database, and that a crashed worker's lease is reclaimed.

use std::time::Duration;

use chrono::Utc;
use engine::lease::claim_one_ready;
use engine::service::Service;
use engine::tick::Tick;
use engine_core::fakes::FakeTaskExecutor;
use sea_orm::{ActiveModelTrait, ActiveValue::Set, EntityTrait};
use serde_json::json;

async fn register_simple(service: &Service) {
    let definition = serde_json::to_string(&engine_core::simple_graph()).expect("serialize");
    service
        .register_graph("simple".to_owned(), &definition)
        .await
        .expect("register");
}

/// `Task -> Wait(Timer{until}) -> End`, registered as graph `timer` v1.
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

/// Seeds a row already parked on the `Wait` node, exactly as a tick that reached it would have
/// left it: `status = waiting`, `wait_kind = {"Timer":{"until":...}}`, `current_nodes` still
/// pointing at the `Wait` node.
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
        status: Set("waiting".to_owned()),
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

    // Simulate worker-1 claiming the execution and then crashing before it ever commits: the
    // lease exists, `status` is "running", but nothing ever released it.
    let claimed = claim_one_ready(
        &test.db,
        "worker-1-about-to-crash",
        Duration::from_millis(1),
    )
    .await
    .expect("claim")
    .expect("claimed");
    assert_eq!(claimed, execution_id);
    tokio::time::sleep(Duration::from_millis(20)).await; // let the 1ms lease expire

    // worker-2 runs a completely ordinary tick — it doesn't know or care that worker-1 died;
    // claim_one_ready's WHERE clause (status='ready' OR expired lease) picks this row up exactly
    // like a fresh one, which is the whole point: recovery is not a special code path.
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

    // Nothing resumes this row: no Interrupt, no Resume, no NOTIFY. claim_one_ready's third
    // disjunct picks it up purely because Timer.until is in the past, and the ordinary tick
    // machinery unparks it by feeding the Wait node a synthetic Ok(Null) output.
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
