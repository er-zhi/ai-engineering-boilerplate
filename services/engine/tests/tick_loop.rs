// Integration tests: the tick loop against a real (testcontainers) Postgres, a fake
// TaskExecutor. Proves what a unit test can't — that claim/run/step/commit actually compose
// correctly against the database, and that a crashed worker's lease is reclaimed.

use std::time::Duration;

use engine::lease::claim_one_ready;
use engine::service::Service;
use engine::tick::Tick;
use engine_core::fakes::FakeTaskExecutor;
use sea_orm::EntityTrait;
use serde_json::json;

async fn register_simple(service: &Service) {
    let definition = serde_json::to_string(&engine_core::simple_graph()).expect("serialize");
    service
        .register_graph("simple".to_owned(), &definition)
        .await
        .expect("register");
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
