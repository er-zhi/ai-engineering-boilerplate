// Test-only Postgres for chat: the shared container bootstrapped with this service's own role
// and schema. Same shape as tool's, knowledge-base's, and llm-router's. Chat has no manual-index
// escape hatch (unlike Tool Service's COALESCE-based unique index), but it does own one
// partitioned table schema-sync cannot express — `chat.events` — so `start` runs the same
// `event_log::setup` the service runs at startup.

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
    let test = common::test_db::start(CHAT).await;
    crate::event_log::setup(&test.db)
        .await
        .expect("create chat.events and its partitions");
    test
}
