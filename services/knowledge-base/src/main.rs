// Stores what crawler (and later, other sources) produce, enriched with LLM-derived metadata and split into embedded passages, and searches them.

mod chunk;
mod embedder_client;
mod entity;
mod ingest;
mod llm_client;
mod llm_router_client;
mod search;
mod service;
mod store;
#[cfg(test)]
mod test_db;

use std::sync::Arc;
use std::time::Duration;

use axum::routing::get;
use common::proto::knowledge_base::v1::{
    IngestRequest, IngestResponse, KnowledgeBaseService, ReadDocumentRequest, ReadDocumentResponse,
    SearchRequest, SearchResponse,
};
use connectrpc::{
    RequestContext, Response, Router as ConnectRouter, ServiceRequest, ServiceResult,
};
use sea_orm::{ConnectionTrait, Database};

use crate::entity::document_chunk::INDEX_STATEMENTS_CREATED_AFTER_SCHEMA_SYNC;
use crate::llm_router_client::LlmRouterClient;
use crate::service::KnowledgeBase;
use crate::store::PgDocuments;

const LLM_ROUTER_CALL_TIMEOUT: Duration = Duration::from_secs(60);
const EMBEDDER_CALL_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_EMBEDDER_URL: &str = "http://host.docker.internal:8086";

struct Ingestor {
    inner: KnowledgeBase<PgDocuments, LlmRouterClient>,
}

#[allow(refining_impl_trait)]
impl KnowledgeBaseService for Ingestor {
    async fn ingest(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, IngestRequest>,
    ) -> ServiceResult<IngestResponse> {
        let response = self.inner.ingest(request.to_owned_message()).await?;
        Response::ok(response)
    }

    async fn search(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, SearchRequest>,
    ) -> ServiceResult<SearchResponse> {
        let response = self.inner.search(request.to_owned_message()).await?;
        Response::ok(response)
    }

    async fn read_document(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, ReadDocumentRequest>,
    ) -> ServiceResult<ReadDocumentResponse> {
        let response = self.inner.read_document(request.to_owned_message()).await?;
        Response::ok(response)
    }
}

fn env(name: &str) -> Result<String, String> {
    std::env::var(name).map_err(|_| format!("{name} is not set"))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    common::logging::init();

    let llm_router_url =
        std::env::var("LLM_ROUTER_URL").unwrap_or_else(|_| "http://127.0.0.1:8083".to_owned());
    let embedder_url =
        std::env::var("EMBEDDER_URL").unwrap_or_else(|_| DEFAULT_EMBEDDER_URL.to_owned());
    let llm = LlmRouterClient::new(
        &llm_router_url,
        LLM_ROUTER_CALL_TIMEOUT,
        &embedder_url,
        EMBEDDER_CALL_TIMEOUT,
    )?;

    let database_url = env("DATABASE_URL")?;
    let db = Database::connect(&database_url).await?;
    db.get_schema_registry("knowledge_base::entity::*")
        .sync(&db)
        .await?;
    for statement in INDEX_STATEMENTS_CREATED_AFTER_SCHEMA_SYNC {
        db.execute_unprepared(statement).await?;
    }

    let ingestor = Ingestor {
        inner: KnowledgeBase::new(PgDocuments::new(db), llm),
    };

    let connect = ConnectRouter::new().add_service(Arc::new(ingestor));

    let app = axum::Router::new()
        .route("/health", get(|| async { "OK" }))
        .fallback_service(connect.into_axum_service());

    let listener = tokio::net::TcpListener::bind("0.0.0.0:8084").await?;
    tracing::info!(
        "knowledge-base listening on 0.0.0.0:8084 -> llm-router at {llm_router_url}, embedder at {embedder_url}"
    );
    axum::serve(listener, app).await?;

    Ok(())
}
