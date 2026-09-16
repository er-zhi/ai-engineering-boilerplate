// tool: the tool registry + executor service. Connects to Postgres, syncs its schema, applies
// the manual uniqueness index, seeds the four system tools if they don't already exist, serves
// Connect RPC.

use std::sync::Arc;

use axum::routing::get;
use common::proto::tools::v1::{
    ActivateToolRequest, ActivateToolResponse, CreateToolRequest, CreateToolResponse,
    ExecuteRequest, ExecuteResponse, ListToolsRequest, ListToolsResponse, Tool as ToolProto,
    ToolService, ValidateToolRequest, ValidateToolResponse,
};
use connectrpc::{
    ConnectError, RequestContext, Response, Router as ConnectRouter, ServiceRequest, ServiceResult,
};
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, ConnectionTrait, Database, EntityTrait,
    QueryFilter,
};

use tool::entity::tool::{Entity, INDEX_STATEMENTS_CREATED_AFTER_SCHEMA_SYNC, Risk, Status};
use tool::service::{ExecuteOutcome, Service};
use tool::slugs;

const TOOL_PORT: &str = "0.0.0.0:8087";

fn env(name: &str) -> Result<String, String> {
    std::env::var(name).map_err(|_| format!("{name} is not set"))
}

fn risk_from_str(value: &str) -> Result<Risk, ConnectError> {
    match value {
        "read_only" => Ok(Risk::ReadOnly),
        "write" => Ok(Risk::Write),
        "destructive" => Ok(Risk::Destructive),
        other => Err(ConnectError::invalid_argument(format!(
            "unknown risk {other:?}"
        ))),
    }
}

fn risk_to_str(risk: Risk) -> &'static str {
    match risk {
        Risk::ReadOnly => "read_only",
        Risk::Write => "write",
        Risk::Destructive => "destructive",
    }
}

fn status_to_str(status: Status) -> &'static str {
    match status {
        Status::Draft => "draft",
        Status::Validated => "validated",
        Status::Active => "active",
    }
}

struct ToolServiceImpl {
    service: Service,
}

#[allow(refining_impl_trait)]
impl ToolService for ToolServiceImpl {
    async fn create_tool(
        &self,
        ctx: RequestContext,
        request: ServiceRequest<'_, CreateToolRequest>,
    ) -> ServiceResult<CreateToolResponse> {
        let msg = request.to_owned_message();
        let Some(principal) = common::principal::from_metadata(ctx.headers()) else {
            return Err(ConnectError::invalid_argument(
                "CreateTool needs a Principal in the request metadata",
            ));
        };
        let input_schema = serde_json::from_str(&msg.input_schema_json)
            .map_err(|e| ConnectError::invalid_argument(e.to_string()))?;
        let output_schema = serde_json::from_str(&msg.output_schema_json)
            .map_err(|e| ConnectError::invalid_argument(e.to_string()))?;
        let risk = risk_from_str(&msg.risk)?;
        let tool_id = self
            .service
            .create_tool(
                Some(principal.user_id),
                msg.slug,
                msg.name,
                msg.description,
                input_schema,
                output_schema,
                risk,
                msg.timeout_seconds,
            )
            .await?;
        Response::ok(CreateToolResponse {
            tool_id: tool_id.to_string(),
            ..Default::default()
        })
    }

    async fn validate_tool(
        &self,
        ctx: RequestContext,
        request: ServiceRequest<'_, ValidateToolRequest>,
    ) -> ServiceResult<ValidateToolResponse> {
        let msg = request.to_owned_message();
        let Some(principal) = common::principal::from_metadata(ctx.headers()) else {
            return Err(ConnectError::invalid_argument(
                "ValidateTool needs a Principal in the request metadata",
            ));
        };
        let tool_id: i64 = msg
            .tool_id
            .parse()
            .map_err(|_| ConnectError::invalid_argument("tool_id is not a valid id"))?;
        let (approved, feedback) = self
            .service
            .validate_tool(tool_id, principal.user_id)
            .await?;
        Response::ok(ValidateToolResponse {
            approved,
            feedback,
            ..Default::default()
        })
    }

    async fn activate_tool(
        &self,
        ctx: RequestContext,
        request: ServiceRequest<'_, ActivateToolRequest>,
    ) -> ServiceResult<ActivateToolResponse> {
        let msg = request.to_owned_message();
        let Some(principal) = common::principal::from_metadata(ctx.headers()) else {
            return Err(ConnectError::invalid_argument(
                "ActivateTool needs a Principal in the request metadata",
            ));
        };
        let tool_id: i64 = msg
            .tool_id
            .parse()
            .map_err(|_| ConnectError::invalid_argument("tool_id is not a valid id"))?;
        self.service
            .activate_tool(tool_id, principal.user_id)
            .await?;
        Response::ok(ActivateToolResponse::default())
    }

    async fn list_tools(
        &self,
        ctx: RequestContext,
        _request: ServiceRequest<'_, ListToolsRequest>,
    ) -> ServiceResult<ListToolsResponse> {
        let user_id = common::principal::from_metadata(ctx.headers()).map(|p| p.user_id);
        let tools = self.service.list_tools(user_id).await?;
        Response::ok(ListToolsResponse {
            tools: tools
                .into_iter()
                .map(|t| ToolProto {
                    id: t.id.to_string(),
                    slug: t.slug,
                    name: t.name,
                    description: t.description,
                    input_schema_json: t.input_schema.to_string(),
                    risk: risk_to_str(t.risk).to_owned(),
                    status: status_to_str(t.status).to_owned(),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        })
    }

    async fn execute(
        &self,
        ctx: RequestContext,
        request: ServiceRequest<'_, ExecuteRequest>,
    ) -> ServiceResult<ExecuteResponse> {
        let msg = request.to_owned_message();
        let caller = common::principal::from_metadata(ctx.headers()).map(|p| p.user_id);
        let outcome = self
            .service
            .execute(caller, &msg.slug, &msg.input_json, &msg.idempotency_key)
            .await?;
        Response::ok(match outcome {
            ExecuteOutcome::Ok(value) => ExecuteResponse {
                status: "ok".to_owned(),
                output_json: value.to_string(),
                ..Default::default()
            },
            ExecuteOutcome::RequiresApproval => ExecuteResponse {
                status: "requires_approval".to_owned(),
                ..Default::default()
            },
            ExecuteOutcome::NotExecutable(message) => ExecuteResponse {
                status: "not_executable".to_owned(),
                error_message: message,
                ..Default::default()
            },
            ExecuteOutcome::Error(message) => ExecuteResponse {
                status: "error".to_owned(),
                error_message: message,
                ..Default::default()
            },
        })
    }
}

async fn seed_system_tools(service: &Service, db: &sea_orm::DatabaseConnection) {
    let system_tools = [
        (
            slugs::WEB_SEARCH,
            "Web search",
            "Searches the web via Brave Search and returns matching pages.",
            Risk::ReadOnly,
        ),
        (
            slugs::WEB_FETCH,
            "Fetch a web page",
            "Fetches a URL and returns its readable title and text.",
            Risk::ReadOnly,
        ),
        (
            slugs::KB_SEARCH,
            "Knowledge base search",
            "Searches this project's knowledge base.",
            Risk::ReadOnly,
        ),
        (
            slugs::KB_READ_DOCUMENT,
            "Read a knowledge base document",
            "Reads the full text of one knowledge base document.",
            Risk::ReadOnly,
        ),
    ];
    for (slug, name, description, risk) in system_tools {
        let exists = Entity::find()
            .filter(tool::entity::tool::Column::UserId.is_null())
            .filter(tool::entity::tool::Column::Slug.eq(slug))
            .one(db)
            .await
            .unwrap_or(None)
            .is_some();
        if exists {
            continue;
        }
        seed_one_system_tool(service, db, slug, name, description, risk).await;
    }
}

/// System tools are pre-vetted Rust implementations, not user-submitted schemas — they skip the
/// Draft -> Validated LLM-review step and go straight to Active.
async fn seed_one_system_tool(
    service: &Service,
    db: &sea_orm::DatabaseConnection,
    slug: &str,
    name: &str,
    description: &str,
    risk: Risk,
) {
    match create_and_activate_system_tool(service, db, slug, name, description, risk).await {
        Ok(()) => tracing::info!(slug, "seeded system tool"),
        Err(error) => tracing::error!(slug, %error, "failed to seed system tool"),
    }
}

async fn create_and_activate_system_tool(
    service: &Service,
    db: &sea_orm::DatabaseConnection,
    slug: &str,
    name: &str,
    description: &str,
    risk: Risk,
) -> Result<(), String> {
    let tool_id = service
        .create_tool(
            None,
            slug.to_owned(),
            name.to_owned(),
            description.to_owned(),
            serde_json::json!({"type": "object"}),
            serde_json::json!({"type": "object"}),
            risk,
            30,
        )
        .await
        .map_err(|error| error.to_string())?;
    let row = Entity::find_by_id(tool_id)
        .one(db)
        .await
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "row vanished before activation".to_owned())?;
    let mut active: tool::entity::tool::ActiveModel = row.into();
    active.status = Set(Status::Active);
    active.update(db).await.map_err(|error| error.to_string())?;
    Ok(())
}

/// sea-orm's schema-sync (2.0.3) always drops an unmatched Postgres unique index via
/// `ALTER TABLE ... DROP CONSTRAINT`, assuming it's constraint-owned — but `tools_owner_slug_idx`
/// is a plain `CREATE UNIQUE INDEX` (the manual COALESCE-based escape hatch this entity's own
/// header comment documents; SeaORM's `unique_key` can't express it). Postgres rejects DROP
/// CONSTRAINT on a plain index (42704 "constraint ... does not exist"), which otherwise crashes
/// this service on every restart after the index first exists. The index itself is untouched by
/// the failed drop; the caller re-asserts it unconditionally right after this call, so it's safe
/// to ignore specifically this known error and continue.
async fn sync_schema(db: &sea_orm::DatabaseConnection) -> Result<(), Box<dyn std::error::Error>> {
    match db.get_schema_registry("tool::entity::*").sync(db).await {
        Ok(()) => Ok(()),
        Err(error) if error.to_string().contains("tools_owner_slug_idx") => {
            tracing::warn!(
                %error,
                "schema-sync tried to drop the manual unique index via DROP CONSTRAINT \
                 (known sea-orm limitation, see sync_schema's doc comment) — ignoring"
            );
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    common::logging::init();

    let database_url = env("DATABASE_URL")?;
    let db = Database::connect(&database_url).await?;
    sync_schema(&db).await?;
    for statement in INDEX_STATEMENTS_CREATED_AFTER_SCHEMA_SYNC {
        db.execute_unprepared(statement).await?;
    }

    let service = Service::new(
        db.clone(),
        &env("LLM_ROUTER_URL")?,
        std::env::var("BRAVE_SEARCH_API_KEY").unwrap_or_default(),
        &env("KNOWLEDGE_BASE_URL")?,
    )?;
    seed_system_tools(&service, &db).await;

    let tool_service = ToolServiceImpl { service };
    let connect = ConnectRouter::new().add_service(Arc::new(tool_service));
    let app = axum::Router::new()
        .route("/health", get(|| async { "OK" }))
        .fallback_service(connect.into_axum_service());

    let listener = tokio::net::TcpListener::bind(TOOL_PORT).await?;
    tracing::info!("tool listening on {TOOL_PORT}");
    axum::serve(listener, app).await?;
    Ok(())
}
