// Test-only Postgres for engine, carrying the schema the running service has.

#![cfg(feature = "test-support")]

use common::test_db::{Entities, ServiceSchema, TestDb};
use sea_orm::ConnectionTrait;

const ENGINE: ServiceSchema = ServiceSchema {
    schema: "engine",
    role: "engine_user",
    password_var: "ENGINE_DB_PASSWORD",
    entities: Entities::Registry("engine::entity::*"),
};

pub async fn start() -> TestDb {
    let test = common::test_db::start(ENGINE).await;
    for statement in crate::execution_event::TABLE_STATEMENTS
        .iter()
        .chain(crate::entity::execution::INDEX_STATEMENTS_CREATED_AFTER_SCHEMA_SYNC.iter())
        .chain(crate::entity::schedule::INDEX_STATEMENTS_CREATED_AFTER_SCHEMA_SYNC.iter())
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
