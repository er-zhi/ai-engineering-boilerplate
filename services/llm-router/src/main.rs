// LLM calls by quality tier: the only service holding the OpenRouter key.

mod adapters;
mod entity;
mod log;
mod provider;
mod router;
mod service;
#[cfg(test)]
mod test_db;
#[cfg(test)]
mod test_provider;
mod tiers;
mod wire;

use std::sync::Arc;
use std::time::Duration;

use axum::routing::get;
use common::proto::llm_router::v1::{
    CompleteRequest, CompleteResponse, DescribeTiersRequest, DescribeTiersResponse,
    LlmRouterService,
};
use connectrpc::{
    RequestContext, Response, Router as ConnectRouter, ServiceRequest, ServiceResult,
};
use sea_orm::Database;

use crate::adapters::openai_compatible::{KeyCheck, OpenAiCompatible};
use crate::log::{PgRequestLog, RequestLog};
use crate::service::Router;
use crate::tiers::Tiers;
use crate::wire::tier_contracts;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const PAYLOAD_CLEANUP_INTERVAL: Duration = Duration::from_secs(3600);

struct LlmRouter {
    inner: Router<OpenAiCompatible, PgRequestLog>,
}

#[allow(refining_impl_trait)]
impl LlmRouterService for LlmRouter {
    async fn complete(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, CompleteRequest>,
    ) -> ServiceResult<CompleteResponse> {
        let response = self.inner.complete(request.to_owned_message()).await?;
        Response::ok(response)
    }

    async fn describe_tiers(
        &self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, DescribeTiersRequest>,
    ) -> ServiceResult<DescribeTiersResponse> {
        Response::ok(DescribeTiersResponse {
            tiers: tier_contracts(),
            ..Default::default()
        })
    }
}

fn env(name: &str) -> Result<String, String> {
    std::env::var(name).map_err(|_| format!("{name} is not set"))
}

async fn prune_payloads_periodically(log: impl RequestLog) {
    let mut interval = tokio::time::interval(PAYLOAD_CLEANUP_INTERVAL);
    loop {
        interval.tick().await;
        prune_expired_payloads(&log).await;
    }
}

async fn prune_expired_payloads(log: &impl RequestLog) {
    let removed = log
        .drop_expired_payloads(chrono::Utc::now())
        .await
        .inspect_err(|error| {
            tracing::error!("could not prune expired llm request payloads: {error}")
        })
        .unwrap_or_default();
    if removed > 0 {
        tracing::info!("pruned {removed} expired llm request payloads");
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    common::logging::init();
    let base_url = std::env::var("OPENROUTER_BASE_URL")
        .unwrap_or_else(|_| "https://openrouter.ai/api/v1".to_owned());
    let api_key = env("OPENROUTER_API_KEY")?;
    let provider = OpenAiCompatible::new(base_url, api_key, REQUEST_TIMEOUT)?;
    match provider.verify_key().await {
        Ok(()) => {}
        Err(KeyCheck::Rejected) => return Err("the provider rejected OPENROUTER_API_KEY".into()),
        Err(KeyCheck::Unreachable(reason)) => {
            tracing::warn!("could not verify OPENROUTER_API_KEY at startup, continuing: {reason}");
        }
    }

    let tiers = Tiers::from_vars(|name| std::env::var(name).ok())?;

    let database_url = env("DATABASE_URL")?;
    let db = Database::connect(&database_url).await?;
    db.get_schema_registry("llm_router::entity::*")
        .sync(&db)
        .await?;

    let log = PgRequestLog::new(db);
    tokio::spawn(prune_payloads_periodically(log.clone()));

    let llm_router = LlmRouter {
        inner: Router::new(provider, log, tiers),
    };

    let connect = ConnectRouter::new().add_service(Arc::new(llm_router));

    let app = axum::Router::new()
        .route("/health", get(|| async { "OK" }))
        .fallback_service(connect.into_axum_service());

    let listener = tokio::net::TcpListener::bind("0.0.0.0:8083").await?;
    tracing::info!("llm-router listening on 0.0.0.0:8083");
    axum::serve(listener, app).await?;

    Ok(())
}
