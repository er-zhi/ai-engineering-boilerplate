// Test-only Postgres: the shared container bootstrapped with chat's own role and schema.

#![cfg(feature = "test-support")]

pub use common::test_db::TestDb;
use common::test_db::{Entities, ServiceSchema};
use sea_orm::ConnectionTrait;

const CHAT: ServiceSchema = ServiceSchema {
    schema: "chat",
    role: "chat_user",
    password_var: "CHAT_DB_PASSWORD",
    entities: Entities::Registry("chat::entity::*"),
};

pub async fn start() -> TestDb {
    let test = common::test_db::start(CHAT).await;
    for statement in crate::entity::topic::INDEX_STATEMENTS_CREATED_AFTER_SCHEMA_SYNC.iter() {
        test.db
            .execute_unprepared(statement)
            .await
            .unwrap_or_else(|error| panic!("index statement failed: {statement}: {error}"));
    }
    crate::event_log::setup(&test.db)
        .await
        .expect("create chat.events and its partitions");
    test
}
