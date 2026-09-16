// chat: session/topic orchestration over Engine. Connects to Postgres, syncs its schema. This
// early version only proves the crate and its entities compile and the schema comes up.

use sea_orm::Database;

fn env(name: &str) -> Result<String, String> {
    std::env::var(name).map_err(|_| format!("{name} is not set"))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    common::logging::init();

    let database_url = env("DATABASE_URL")?;
    let db = Database::connect(&database_url).await?;
    db.get_schema_registry("chat::entity::*").sync(&db).await?;

    tracing::info!("chat: schema synced, no RPC surface yet");
    Ok(())
}
