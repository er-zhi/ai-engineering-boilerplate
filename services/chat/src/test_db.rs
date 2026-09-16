// Test-only Postgres for chat: the shared container bootstrapped with this service's own role
// and schema. Same shape as tool's, knowledge-base's, and llm-router's. Chat has no manual-index
// escape hatch (unlike Tool Service's COALESCE-based unique index), so no post-sync index
// statements are needed here.

#![cfg(feature = "test-support")]

use common::test_db::ServiceSchema;
pub use common::test_db::TestDb;

const CHAT: ServiceSchema = ServiceSchema {
    schema: "chat",
    role: "chat_user",
    password_var: "CHAT_DB_PASSWORD",
    entity_prefix: "chat::entity::*",
};

pub async fn start() -> TestDb {
    common::test_db::start(CHAT).await
}
