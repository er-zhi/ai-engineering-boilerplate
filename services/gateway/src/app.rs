// Builds Gateway routes after configuration and clients are ready.

use std::sync::Arc;

use axum::http::{HeaderValue, header};
use axum::middleware;
use axum::routing::{get, get_service, post};
use connectrpc::Router as ConnectRouter;
use sea_orm::{Database, DatabaseConnection};
use tower_http::services::ServeFile;
use tower_http::set_header::SetResponseHeaderLayer;

use crate::config::GatewayConfig;
use crate::proxy::Gateway;
use crate::sessions::PgCacheStore;
use crate::web::{
    Auth, login_submit, logout, require_session, sweep_expired_sessions_periodically,
};

const PAGE_CACHE_HEADER: &str = "no-store";

pub async fn application(
    config: &GatewayConfig,
) -> Result<axum::Router, Box<dyn std::error::Error>> {
    let db = Database::connect(config.database_url()).await?;
    db.get_schema_registry("gateway::entity::*")
        .sync(&db)
        .await?;
    Ok(routes(config, db))
}

fn routes(config: &GatewayConfig, db: DatabaseConnection) -> axum::Router {
    let sessions = PgCacheStore::new(db);
    tokio::spawn(sweep_expired_sessions_periodically(sessions.clone()));
    let auth = Auth::new(sessions, config.password().to_owned(), config.session_ttl());
    let gateway = Arc::new(Gateway::new(
        config.crawler_url().clone(),
        config.knowledge_base_url().clone(),
        config.chat_url().clone(),
    ));
    let connect = ConnectRouter::new()
        .add_service::<_, common::proto::crawler::v1::CrawlerServiceRegisterMarker>(Arc::clone(
            &gateway,
        ))
        .add_service::<_, common::proto::knowledge_base::v1::KnowledgeBaseServiceRegisterMarker>(
            Arc::clone(&gateway),
        )
        .add_service::<_, common::proto::chat::v1::ChatServiceRegisterMarker>(gateway);

    let protected = axum::Router::new()
        .route_service(
            "/",
            ServeFile::new(config.frontend_dir().join("index.html")),
        )
        .route_service(
            "/sources",
            ServeFile::new(config.frontend_dir().join("sources.html")),
        )
        .route_service(
            "/chat",
            ServeFile::new(config.frontend_dir().join("chat.html")),
        )
        .layer(SetResponseHeaderLayer::if_not_present(
            header::CACHE_CONTROL,
            HeaderValue::from_static(PAGE_CACHE_HEADER),
        ))
        .fallback_service(connect.into_axum_service())
        .layer(middleware::from_fn_with_state(
            auth.clone(),
            require_session,
        ));
    let public = axum::Router::new()
        .route("/health", get(|| async { "OK" }))
        .route(
            "/login",
            get_service(ServeFile::new(config.frontend_dir().join("login.html")))
                .post(login_submit),
        )
        .route("/logout", post(logout))
        .with_state(auth);
    public.merge(protected)
}

#[cfg(test)]
mod tests;
