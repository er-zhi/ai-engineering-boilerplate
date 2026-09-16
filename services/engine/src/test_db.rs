// Test-only Postgres for engine: the shared container bootstrapped with this service's own role
// and schema, plus its startup indexes — same shape as knowledge-base's and llm-router's.

#![cfg(feature = "test-support")]

use common::test_db::{ServiceSchema, TestDb};
use sea_orm::ConnectionTrait;

use crate::entity::execution_event::INDEX_STATEMENTS_CREATED_AFTER_SCHEMA_SYNC;

const ENGINE: ServiceSchema = ServiceSchema {
    schema: "engine",
    role: "engine_user",
    password_var: "ENGINE_DB_PASSWORD",
    entity_prefix: "engine::entity::*",
};

pub async fn start() -> TestDb {
    let test = common::test_db::start(ENGINE).await;
    for statement in INDEX_STATEMENTS_CREATED_AFTER_SCHEMA_SYNC {
        test.db
            .execute_unprepared(statement)
            .await
            .unwrap_or_else(|error| panic!("index statement failed: {statement}: {error}"));
    }
    test
}
