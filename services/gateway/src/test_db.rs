// Test-only Postgres for gateway: the shared container bootstrapped with this service's role and schema.

use common::test_db::{Entities, ServiceSchema, TestDb};

const GATEWAY: ServiceSchema = ServiceSchema {
    schema: "gateway",
    role: "gateway_user",
    password_var: "GATEWAY_DB_PASSWORD",
    entities: Entities::Registry("gateway::entity::*"),
};

pub async fn start() -> TestDb {
    common::test_db::start(GATEWAY).await
}
