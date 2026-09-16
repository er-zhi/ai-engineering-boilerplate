// Integration tests for engine.execution_events' monthly partitioning (see execution_event.rs and
// partition.rs). Kept out of tick_loop.rs on purpose: nothing here runs a tick — these are about
// the table's shape, which rows land in which child, and the fact that StreamEvents cannot tell
// the difference.

use chrono::{DateTime, TimeZone, Utc};
use engine::execution_event;
use engine::partition;
use futures::StreamExt;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, ConnectionTrait, DatabaseConnection,
    DbBackend, EntityTrait, QueryFilter, QueryOrder, Statement,
};
use uuid::Uuid;

fn at(year: i32, month: u32, day: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(year, month, day, 12, 0, 0)
        .single()
        .expect("a real instant")
}

/// Inserts one event at an explicit `occurred_at`, through the ordinary entity — the same path
/// `wire::event_to_active_model` uses, so this exercises the real insert, not hand-written SQL.
async fn insert_event_at(
    db: &DatabaseConnection,
    execution_id: Uuid,
    occurred_at: DateTime<Utc>,
    kind: &str,
) -> i64 {
    execution_event::ActiveModel {
        event_id: Set(Uuid::new_v4()),
        execution_id: Set(execution_id),
        user_id: Set(None),
        version: Set(1),
        causation_id: Set(None),
        occurred_at: Set(occurred_at),
        payload: Set(serde_json::json!({ kind: {} })),
        ..Default::default()
    }
    .insert(db)
    .await
    .expect("insert event")
    .id
}

/// Which child partition a row physically lives in, straight from Postgres (unqualified: the
/// engine role's search_path already starts at `engine`).
async fn partition_of(db: &DatabaseConnection, id: i64) -> String {
    db.query_one_raw(Statement::from_string(
        DbBackend::Postgres,
        format!(
            "SELECT tableoid::regclass::text AS name FROM engine.execution_events WHERE id = {id}"
        ),
    ))
    .await
    .expect("query")
    .expect("the row exists")
    .try_get("", "name")
    .expect("a partition name")
}

#[tokio::test(flavor = "multi_thread")]
async fn the_event_log_is_a_partitioned_parent() {
    let test = engine::test_db::start().await;

    assert!(
        partition::is_partitioned(&test.db)
            .await
            .expect("ask the database"),
        "the table schema-sync never created is the partitioned one"
    );
    // The behaviour that makes partition maintenance load-bearing rather than cosmetic: a month
    // with no partition has nowhere to put the row, and the insert itself fails. This is why
    // `ensure_current_and_next` always opens the month ahead.
    let uncovered = execution_event::ActiveModel {
        event_id: Set(Uuid::new_v4()),
        execution_id: Set(Uuid::new_v4()),
        user_id: Set(None),
        version: Set(1),
        causation_id: Set(None),
        occurred_at: Set(at(2031, 3, 15)),
        payload: Set(serde_json::json!({"NodeStarted": {}})),
        ..Default::default()
    }
    .insert(&test.db)
    .await;
    assert!(
        uncovered.is_err(),
        "an event in a month with no partition has nowhere to land"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn events_from_two_months_land_in_two_partitions() {
    let test = engine::test_db::start().await;
    let execution_id = Uuid::new_v4();
    // Two months well away from "now", so this does not depend on which month the suite runs in.
    for month in [at(2031, 3, 15), at(2031, 4, 2)] {
        partition::ensure_partition(&test.db, month)
            .await
            .expect("create the partition");
    }

    let march = insert_event_at(&test.db, execution_id, at(2031, 3, 15), "NodeStarted").await;
    let april = insert_event_at(&test.db, execution_id, at(2031, 4, 2), "NodeCompleted").await;

    assert_eq!(
        partition_of(&test.db, march).await,
        "execution_events_y2031m03"
    );
    assert_eq!(
        partition_of(&test.db, april).await,
        "execution_events_y2031m04"
    );
    assert!(
        april > march,
        "one shared sequence, so id still orders by insertion"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn reads_and_streams_cross_the_partition_boundary_unchanged() {
    let test = engine::test_db::start().await;
    let execution_id = Uuid::new_v4();
    for month in [at(2031, 3, 15), at(2031, 4, 2)] {
        partition::ensure_partition(&test.db, month)
            .await
            .expect("create the partition");
    }
    // Inserted newest-month-first, so anything ordering by occurred_at rather than id would show
    // these the other way round.
    insert_event_at(&test.db, execution_id, at(2031, 4, 2), "NodeStarted").await;
    insert_event_at(&test.db, execution_id, at(2031, 3, 15), "NodeCompleted").await;
    insert_event_at(&test.db, execution_id, at(2031, 4, 3), "ExecutionCompleted").await;

    // The parent's public interface is unchanged: same columns, same `id` ordering column, so
    // stream.rs's query needed no edit at all — this proves that rather than assuming it.
    let rows = execution_event::Entity::find()
        .filter(execution_event::Column::ExecutionId.eq(execution_id))
        .order_by_asc(execution_event::Column::Id)
        .all(&test.db)
        .await
        .expect("read across partitions");
    assert_eq!(rows.len(), 3, "a query on the parent sees every partition");

    let mut stream = engine::stream::stream_events(test.db.clone(), Some(execution_id), None);
    let mut kinds = Vec::new();
    while let Some(event) = stream.next().await {
        kinds.push(event.expect("ok").payload_kind);
    }
    assert_eq!(
        kinds,
        vec!["NodeStarted", "NodeCompleted", "ExecutionCompleted"],
        "StreamEvents still yields insertion (id) order across the month boundary, and still \
         closes on the terminal event"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn maintenance_keeps_the_window_ahead_open_and_drops_only_past_the_retention() {
    let test = engine::test_db::start().await;
    let now = at(2031, 6, 10);
    // Three months of history, each with an event in it, so what maintenance does is visible as
    // rows appearing and disappearing rather than as catalog rows.
    let execution_id = Uuid::new_v4();
    for month in [at(2031, 1, 5), at(2031, 4, 5), at(2031, 5, 5)] {
        partition::ensure_partition(&test.db, month)
            .await
            .expect("create the partition");
        insert_event_at(&test.db, execution_id, month, "NodeStarted").await;
    }

    // ENGINE_EVENTS_RETENTION_MONTHS is unset by default, so this is the path the running service
    // does not take today — exercised here with an explicit value so it is not dead code.
    partition::maintain(&test.db, now, Some(3))
        .await
        .expect("maintain");

    assert_eq!(
        months_with_events(&test.db, execution_id).await,
        vec![
            at(2031, 4, 5).format("%Y-%m").to_string(),
            at(2031, 5, 5).format("%Y-%m").to_string()
        ],
        "three months of retention keeps April and May and drops January with its whole partition"
    );
    // And the window ahead is open: this month and next month accept inserts, which is the only
    // thing "the partition exists" means to a writer.
    for month in [now, at(2031, 7, 20)] {
        insert_event_at(&test.db, execution_id, month, "NodeCompleted").await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn maintenance_without_a_retention_drops_nothing() {
    let test = engine::test_db::start().await;
    let execution_id = Uuid::new_v4();
    partition::ensure_partition(&test.db, at(2031, 1, 5))
        .await
        .expect("create the partition");
    insert_event_at(&test.db, execution_id, at(2031, 1, 5), "NodeStarted").await;

    partition::maintain(&test.db, at(2031, 6, 10), None)
        .await
        .expect("maintain");

    assert_eq!(
        months_with_events(&test.db, execution_id).await,
        vec!["2031-01".to_owned()],
        "unset retention means the log is kept forever"
    );
}

/// The months this execution still has events in, oldest first — the readable form of "which
/// partitions survived", asserted through the entity rather than the catalog.
async fn months_with_events(db: &DatabaseConnection, execution_id: Uuid) -> Vec<String> {
    execution_event::Entity::find()
        .filter(execution_event::Column::ExecutionId.eq(execution_id))
        .order_by_asc(execution_event::Column::OccurredAt)
        .all(db)
        .await
        .expect("read events")
        .into_iter()
        .map(|row| row.occurred_at.format("%Y-%m").to_string())
        .collect()
}
