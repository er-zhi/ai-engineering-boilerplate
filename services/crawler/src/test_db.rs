// Test-only Postgres for the crawler: the shared container bootstrapped with this service's role and schema.

use common::test_db::{ServiceSchema, TestDb};

const CRAWLER: ServiceSchema = ServiceSchema {
    schema: "crawler",
    role: "crawler_user",
    password_var: "CRAWLER_DB_PASSWORD",
    entity_prefix: "crawler::entity::*",
};

pub async fn start() -> TestDb {
    common::test_db::start(CRAWLER).await
}
