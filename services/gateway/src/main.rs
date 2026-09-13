// Starts the public Gateway after configuration has been validated.

mod app;
mod auth;
mod config;
mod entity;
mod proxy;
mod sessions;
#[cfg(test)]
mod test_db;
mod web;

use crate::config::gateway_config;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    common::logging::init();
    let config = gateway_config()?;
    let app = app::application(&config).await?;
    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await?;
    tracing::info!(
        "gateway listening on 0.0.0.0:8080 -> crawler at {}, knowledge-base at {}",
        config.crawler_url(),
        config.knowledge_base_url()
    );
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    include!("main_tests.rs");
}
