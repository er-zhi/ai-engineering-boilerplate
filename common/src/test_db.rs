// Test-only Postgres: a throwaway pgvector container, bootstrapped with one service's role and schema the way compose.yaml bootstraps the stack.

use std::time::Duration;

use sea_orm::{Database, DatabaseConnection};
use testcontainers::core::{Healthcheck, IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, CopyTargetOptions, GenericImage, ImageExt};

const BOOTSTRAP_PATH: &str = "/docker-entrypoint-initdb.d/init.sql";
const HEALTH_CHECK_INTERVAL: Duration = Duration::from_millis(250);
const HEALTH_CHECK_RETRIES: u32 = 120;
const PASSWORD: &str = "test";
const DATABASE: &str = "app";

pub struct ServiceSchema {
    pub schema: &'static str,
    pub role: &'static str,
    pub password_var: &'static str,
    pub entity_prefix: &'static str,
}

pub struct TestDb {
    pub db: DatabaseConnection,
    _container: ContainerAsync<GenericImage>,
}

pub async fn start(service: ServiceSchema) -> TestDb {
    let bootstrap_sql = format!(
        "CREATE EXTENSION IF NOT EXISTS vector;\n\
         \\getenv password {var}\n\
         CREATE ROLE {role} LOGIN PASSWORD :'password';\n\
         CREATE SCHEMA {schema} AUTHORIZATION {role};\n\
         ALTER ROLE {role} SET search_path TO {schema}, public;\n",
        var = service.password_var,
        role = service.role,
        schema = service.schema,
    );
    let role_accepts_tcp = format!("pg_isready -h 127.0.0.1 -U {} -d {DATABASE}", service.role);

    let container = GenericImage::new("pgvector/pgvector", "pg18")
        .with_exposed_port(5432.tcp())
        .with_wait_for(WaitFor::healthcheck())
        .with_health_check(
            Healthcheck::cmd_shell(role_accepts_tcp)
                .with_interval(HEALTH_CHECK_INTERVAL)
                .with_retries(HEALTH_CHECK_RETRIES),
        )
        .with_copy_to(
            CopyTargetOptions::new(BOOTSTRAP_PATH),
            bootstrap_sql.into_bytes(),
        )
        .with_env_var("POSTGRES_PASSWORD", PASSWORD)
        .with_env_var("POSTGRES_DB", DATABASE)
        .with_env_var(service.password_var, PASSWORD)
        .start()
        .await
        .expect("start Postgres; database tests need a running Docker");
    let host = container.get_host().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();

    let role_url = format!(
        "postgres://{}:{PASSWORD}@{host}:{port}/{DATABASE}",
        service.role
    );
    let db = Database::connect(role_url).await.unwrap();
    db.get_schema_registry(service.entity_prefix)
        .sync(&db)
        .await
        .unwrap();

    TestDb {
        db,
        _container: container,
    }
}
