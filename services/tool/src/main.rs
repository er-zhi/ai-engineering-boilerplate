// tool: the tool registry + executor service. Connects to Postgres, syncs its schema, applies
// the one manual uniqueness index schema-sync cannot express. This early version only proves
// the crate and its entity compile and the schema comes up.

use sea_orm::{ConnectionTrait, Database};

use tool::entity::tool::INDEX_STATEMENTS_CREATED_AFTER_SCHEMA_SYNC;

fn env(name: &str) -> Result<String, String> {
    std::env::var(name).map_err(|_| format!("{name} is not set"))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    common::logging::init();

    let database_url = env("DATABASE_URL")?;
    let db = Database::connect(&database_url).await?;
    db.get_schema_registry("tool::entity::*").sync(&db).await?;
    for statement in INDEX_STATEMENTS_CREATED_AFTER_SCHEMA_SYNC {
        db.execute_unprepared(statement).await?;
    }

    tracing::info!("tool: schema synced, no RPC surface yet");
    Ok(())
}
