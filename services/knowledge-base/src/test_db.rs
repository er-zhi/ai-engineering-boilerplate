// Test-only Postgres for knowledge-base: the shared container bootstrapped with this service's role and schema, plus its startup indexes.

use common::test_db::{Entities, ServiceSchema, TestDb};
use sea_orm::ConnectionTrait;

use crate::entity::document_chunk::INDEX_STATEMENTS_CREATED_AFTER_SCHEMA_SYNC;

const KNOWLEDGE_BASE: ServiceSchema = ServiceSchema {
    schema: "knowledge_base",
    role: "knowledge_base_user",
    password_var: "KNOWLEDGE_BASE_DB_PASSWORD",
    entities: Entities::Registry("knowledge_base::entity::*"),
};

pub async fn start() -> TestDb {
    let test = common::test_db::start(KNOWLEDGE_BASE).await;
    for statement in INDEX_STATEMENTS_CREATED_AFTER_SCHEMA_SYNC {
        test.db
            .execute_unprepared(statement)
            .await
            .unwrap_or_else(|error| panic!("index statement failed: {statement}: {error}"));
    }
    test
}
