// Test-only Postgres for tool: the shared container with this service's role, schema and indexes.

#![cfg(feature = "test-support")]

pub use common::test_db::TestDb;
use common::test_db::{Entities, ServiceSchema};
use sea_orm::ConnectionTrait;

const TOOL: ServiceSchema = ServiceSchema {
    schema: "tool",
    role: "tool_user",
    password_var: "TOOL_DB_PASSWORD",
    entities: Entities::Registry("tool::entity::*"),
};

pub async fn start() -> TestDb {
    let test = common::test_db::start(TOOL).await;
    for statement in crate::entity::tool::INDEX_STATEMENTS_CREATED_AFTER_SCHEMA_SYNC {
        test.db
            .execute_unprepared(statement)
            .await
            .unwrap_or_else(|error| panic!("index statement failed: {statement}: {error}"));
    }
    test
}
