// Test-only Postgres: a throwaway pgvector container, bootstrapped with the crawler's role and schema the way compose.yaml bootstraps the stack.

use std::time::Duration;

use sea_orm::{Database, DatabaseConnection};
use testcontainers::core::{Healthcheck, IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, CopyTargetOptions, GenericImage, ImageExt};

const BOOTSTRAP_SQL: &str = r"CREATE EXTENSION IF NOT EXISTS vector;
\getenv crawler_password CRAWLER_DB_PASSWORD
CREATE ROLE crawler_user LOGIN PASSWORD :'crawler_password';
CREATE SCHEMA crawler AUTHORIZATION crawler_user;
ALTER ROLE crawler_user SET search_path TO crawler, public;
";
const BOOTSTRAP_PATH: &str = "/docker-entrypoint-initdb.d/init.sql";
const REAL_SERVER_ACCEPTS_CRAWLER_ROLE_OVER_TCP: &str =
    "pg_isready -h 127.0.0.1 -U crawler_user -d app";
const HEALTH_CHECK_INTERVAL: Duration = Duration::from_millis(250);
const HEALTH_CHECK_RETRIES: u32 = 120;
const PASSWORD: &str = "test";

pub struct TestDb {
    pub db: DatabaseConnection,
    _container: ContainerAsync<GenericImage>,
}

pub async fn start() -> TestDb {
    let container = GenericImage::new("pgvector/pgvector", "pg18")
        .with_exposed_port(5432.tcp())
        .with_wait_for(WaitFor::healthcheck())
        .with_health_check(
            Healthcheck::cmd_shell(REAL_SERVER_ACCEPTS_CRAWLER_ROLE_OVER_TCP)
                .with_interval(HEALTH_CHECK_INTERVAL)
                .with_retries(HEALTH_CHECK_RETRIES),
        )
        .with_copy_to(
            CopyTargetOptions::new(BOOTSTRAP_PATH),
            BOOTSTRAP_SQL.as_bytes().to_vec(),
        )
        .with_env_var("POSTGRES_PASSWORD", PASSWORD)
        .with_env_var("POSTGRES_DB", "app")
        .with_env_var("CRAWLER_DB_PASSWORD", PASSWORD)
        .start()
        .await
        .expect("start Postgres; database tests need a running Docker");
    let host = container.get_host().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();

    let crawler_role_url = format!("postgres://crawler_user:{PASSWORD}@{host}:{port}/app");
    let db = Database::connect(crawler_role_url).await.unwrap();
    db.get_schema_registry("crawler::entity::*")
        .sync(&db)
        .await
        .unwrap();

    TestDb {
        db,
        _container: container,
    }
}
