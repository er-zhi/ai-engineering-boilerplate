// Test-only Postgres for the router: the shared container bootstrapped with this service's role and schema.

use common::test_db::{ServiceSchema, TestDb};

const LLM_ROUTER: ServiceSchema = ServiceSchema {
    schema: "llm_router",
    role: "llm_router_user",
    password_var: "LLM_ROUTER_DB_PASSWORD",
    entity_prefix: "llm_router::entity::*",
};

pub async fn start() -> TestDb {
    common::test_db::start(LLM_ROUTER).await
}
