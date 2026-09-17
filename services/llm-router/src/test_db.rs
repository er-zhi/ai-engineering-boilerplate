// Test-only Postgres for the router: the shared container bootstrapped with this service's role and schema.

use common::test_db::{Entities, ServiceSchema, TestDb};

// Every llm_router table is a partitioned parent created by the literal DDL in partition.rs, which the
// schema registry cannot express, so this service has nothing for the registry to sync and says so.
const LLM_ROUTER: ServiceSchema = ServiceSchema {
    schema: "llm_router",
    role: "llm_router_user",
    password_var: "LLM_ROUTER_DB_PASSWORD",
    entities: Entities::CreatedByTheService,
};

pub async fn start() -> TestDb {
    let test = common::test_db::start(LLM_ROUTER).await;
    crate::partition::create_parents(&test.db)
        .await
        .expect("create the llm_router audit tables");
    crate::partition::maintain(&test.db, chrono::Utc::now())
        .await
        .expect("open this month's and next month's audit partitions");
    test
}
