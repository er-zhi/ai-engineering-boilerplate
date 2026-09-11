// Test-only Postgres: a throwaway pgvector container bootstrapped by infra/postgres/init.sh.

use std::time::Duration;

use sea_orm::{Database, DatabaseConnection};
use testcontainers::core::{Healthcheck, IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, CopyTargetOptions, GenericImage, ImageExt};

const PRODUCTION_BOOTSTRAP_SCRIPT: &[u8] = include_bytes!("../../../infra/postgres/init.sh");
const INIT_SCRIPT_PATH: &str = "/docker-entrypoint-initdb.d/init.sh";
const EXECUTABLE: u32 = 0o755;
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
            CopyTargetOptions::new(INIT_SCRIPT_PATH).with_mode(EXECUTABLE),
            PRODUCTION_BOOTSTRAP_SCRIPT.to_vec(),
        )
        .with_env_var("POSTGRES_PASSWORD", PASSWORD)
        .with_env_var("POSTGRES_DB", "app")
        .with_env_var("CRAWLER_DB_PASSWORD", PASSWORD)
        .with_env_var("GATEWAY_DB_PASSWORD", PASSWORD)
        .with_env_var("LLM_ROUTER_DB_PASSWORD", PASSWORD)
        .start()
        .await
        .expect("start Postgres; database tests need Docker (see scripts/test.sh)");
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
