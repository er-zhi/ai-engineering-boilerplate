// Test-only Postgres for engine: the shared container bootstrapped with this service's own role
// and schema, plus everything main.rs does to the schema after schema-sync — the partitioned
// execution_events parent (schema-sync never creates it, see execution_event.rs), this month's
// and next month's partitions, and the startup indexes — so a test sees the same schema the
// running service does. Same shape as knowledge-base's and llm-router's.

#![cfg(feature = "test-support")]

use common::test_db::{ServiceSchema, TestDb};
use sea_orm::ConnectionTrait;

const ENGINE: ServiceSchema = ServiceSchema {
    schema: "engine",
    role: "engine_user",
    password_var: "ENGINE_DB_PASSWORD",
    entity_prefix: "engine::entity::*",
};

pub async fn start() -> TestDb {
    let test = common::test_db::start(ENGINE).await;
    for statement in crate::execution_event::TABLE_STATEMENTS
        .iter()
        .chain(crate::entity::execution::INDEX_STATEMENTS_CREATED_AFTER_SCHEMA_SYNC.iter())
        .chain(crate::execution_event::INDEX_STATEMENTS_CREATED_AFTER_SCHEMA_SYNC.iter())
    {
        test.db
            .execute_unprepared(statement)
            .await
            .unwrap_or_else(|error| panic!("startup statement failed: {statement}: {error}"));
    }
    crate::partition::ensure_current_and_next(&test.db, chrono::Utc::now())
        .await
        .expect("create this month's and next month's event partitions");
    test
}
