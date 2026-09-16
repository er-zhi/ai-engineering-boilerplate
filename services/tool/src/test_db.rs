// Test-only Postgres for tool: the shared container bootstrapped with this service's own role
// and schema, plus the startup index sea-orm's schema-sync can't express (see
// entity/tool.rs's header comment). Same shape as knowledge-base's and llm-router's.

#![cfg(feature = "test-support")]

use common::test_db::ServiceSchema;
pub use common::test_db::TestDb;
use sea_orm::ConnectionTrait;

const TOOL: ServiceSchema = ServiceSchema {
    schema: "tool",
    role: "tool_user",
    password_var: "TOOL_DB_PASSWORD",
    entity_prefix: "tool::entity::*",
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
