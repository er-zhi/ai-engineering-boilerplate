// Web UI. Owns every page and all client-side logic; Gateway proxies browser page loads here.

use axum::response::Html;
use axum::routing::get;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let app = axum::Router::new()
        .route(
            "/",
            get(|| async { Html(include_str!("../client/index.html")) }),
        )
        .route("/health", get(|| async { "OK" }));

    let listener = tokio::net::TcpListener::bind("0.0.0.0:8082").await?;
    println!("frontend listening on 0.0.0.0:8082");
    axum::serve(listener, app).await?;

    Ok(())
}
