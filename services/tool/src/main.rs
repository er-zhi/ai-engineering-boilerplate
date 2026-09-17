// tool: the tool registry and executor service — schema, system-tool seeding and Connect RPC.

use std::sync::Arc;

use axum::routing::get;
use buffa::EnumValue;
use common::proto::tools::v1::{
    ActivateToolRequest, ActivateToolResponse, CreateToolRequest, CreateToolResponse,
    ExecuteRequest, ExecuteResponse, ExecuteStatus, ListToolsRequest, ListToolsResponse,
    Tool as ToolProto, ToolRisk, ToolService, ToolStatus, ValidateToolRequest,
    ValidateToolResponse,
};
use connectrpc::{
    ConnectError, RequestContext, Response, Router as ConnectRouter, ServiceRequest, ServiceResult,
};
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, ConnectionTrait, Database, EntityTrait,
    QueryFilter,
};

use tool::entity::tool::{
    Entity, INDEX_STATEMENTS_CREATED_AFTER_SCHEMA_SYNC, MANUAL_UNIQUE_INDEX_NAME, Risk, Status,
};
use tool::service::{ExecuteOutcome, NewTool, Service};
use tool::slugs;

const TOOL_PORT: &str = "0.0.0.0:8087";
const SYSTEM_TOOL_TIMEOUT_SECONDS: i32 = 30;

fn env(name: &str) -> Result<String, String> {
    std::env::var(name).map_err(|_| format!("{name} is not set"))
}

fn risk_from_proto(risk: EnumValue<ToolRisk>) -> Result<Risk, ConnectError> {
    match risk {
        EnumValue::Known(ToolRisk::ReadOnly) => Ok(Risk::ReadOnly),
        EnumValue::Known(ToolRisk::Write) => Ok(Risk::Write),
        EnumValue::Known(ToolRisk::Destructive) => Ok(Risk::Destructive),
        other => Err(ConnectError::invalid_argument(format!(
            "risk must be one of READ_ONLY, WRITE, DESTRUCTIVE, got {other:?}"
        ))),
    }
}

fn risk_to_proto(risk: Risk) -> ToolRisk {
    match risk {
        Risk::ReadOnly => ToolRisk::ReadOnly,
        Risk::Write => ToolRisk::Write,
        Risk::Destructive => ToolRisk::Destructive,
    }
}

fn status_to_proto(status: Status) -> ToolStatus {
    match status {
        Status::Draft => ToolStatus::Draft,
        Status::Validated => ToolStatus::Validated,
        Status::Active => ToolStatus::Active,
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
        let new_tool = NewTool::checked(
            msg.slug,
            msg.name,
            msg.description,
            input_schema,
            output_schema,
            risk_from_proto(msg.risk)?,
            msg.timeout_seconds,
        )?;
        let tool_id = self
            .service
            .create_tool(Some(principal.user_id), new_tool)
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
                    risk: EnumValue::Known(risk_to_proto(t.risk)),
                    status: EnumValue::Known(status_to_proto(t.status)),
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
                status: EnumValue::Known(ExecuteStatus::Ok),
                output_json: value.to_string(),
                ..Default::default()
            },
            ExecuteOutcome::RequiresApproval => ExecuteResponse {
                status: EnumValue::Known(ExecuteStatus::RequiresApproval),
                ..Default::default()
            },
            ExecuteOutcome::NotExecutable(message) => ExecuteResponse {
                status: EnumValue::Known(ExecuteStatus::NotExecutable),
                error_message: message,
                ..Default::default()
            },
            ExecuteOutcome::Error(message) => ExecuteResponse {
                status: EnumValue::Known(ExecuteStatus::Error),
                error_message: message,
                ..Default::default()
            },
        })
    }
}

fn system_tool_definitions() -> [SystemTool; 4] {
    [
        SystemTool {
            slug: slugs::WEB_SEARCH,
            name: "Web search",
            description: "Searches the public web and returns matching pages with a title, URL and snippet.",
            risk: Risk::ReadOnly,
            input_schema: tool::args::input_schema::<tool::args::Search>(),
        },
        SystemTool {
            slug: slugs::WEB_FETCH,
            name: "Fetch a web page",
            description: "Fetches a page and returns its readable title and text. Pass several urls to fetch candidates at once and get whichever answers first — do that when several pages would each answer the question and you cannot tell which will respond.",
            risk: Risk::ReadOnly,
            input_schema: tool::args::input_schema::<tool::args::Fetch>(),
        },
        SystemTool {
            slug: slugs::KB_SEARCH,
            name: "Knowledge base search",
            description: "Searches this project's knowledge base.",
            risk: Risk::ReadOnly,
            input_schema: tool::args::input_schema::<tool::args::Search>(),
        },
        SystemTool {
            slug: slugs::KB_READ_DOCUMENT,
            name: "Read a knowledge base document",
            description: "Reads the full text of one knowledge base document, identified by the source and source_id a knowledge base search result carries.",
            risk: Risk::ReadOnly,
            input_schema: tool::args::input_schema::<tool::args::ReadDocument>(),
        },
    ]
}

async fn seed_system_tools(service: &Service, db: &sea_orm::DatabaseConnection) {
    for definition in system_tool_definitions() {
        let slug = definition.slug;
        let existing = match Entity::find()
            .filter(tool::entity::tool::Column::UserId.is_null())
            .filter(tool::entity::tool::Column::Slug.eq(slug))
            .one(db)
            .await
        {
            Ok(row) => row,
            Err(error) => {
                tracing::error!(slug, %error, "could not look up a system tool, skipping seeding");
                continue;
            }
        };
        if seeding_already_finished(existing.as_ref(), &definition) {
            continue;
        }
        seed_pre_vetted_system_tool(service, db, definition, existing).await;
    }
}

fn seeding_already_finished(
    existing: Option<&tool::entity::tool::Model>,
    definition: &SystemTool,
) -> bool {
    existing.is_some_and(|row| {
        row.status == Status::Active
            && row.name == definition.name
            && row.description == definition.description
            && row.input_schema == definition.input_schema
    })
}

struct SystemTool {
    slug: &'static str,
    name: &'static str,
    description: &'static str,
    risk: Risk,
    input_schema: serde_json::Value,
}

async fn seed_pre_vetted_system_tool(
    service: &Service,
    db: &sea_orm::DatabaseConnection,
    definition: SystemTool,
    existing: Option<tool::entity::tool::Model>,
) {
    let slug = definition.slug;
    let result = match existing {
        Some(stored) => activate_without_llm_review(db, stored, &definition).await,
        None => create_and_activate_system_tool(service, db, &definition).await,
    };
    match result {
        Ok(()) => tracing::info!(slug, "seeded system tool"),
        Err(error) => tracing::error!(slug, %error, "failed to seed system tool"),
    }
}

async fn create_and_activate_system_tool(
    service: &Service,
    db: &sea_orm::DatabaseConnection,
    definition: &SystemTool,
) -> Result<(), String> {
    let new_tool = NewTool::checked(
        definition.slug.to_owned(),
        definition.name.to_owned(),
        definition.description.to_owned(),
        definition.input_schema.clone(),
        serde_json::json!({"type": "object"}),
        definition.risk,
        SYSTEM_TOOL_TIMEOUT_SECONDS,
    )
    .map_err(|error| error.to_string())?;
    let tool_id = service
        .create_tool(None, new_tool)
        .await
        .map_err(|error| error.to_string())?;
    let row = Entity::find_by_id(tool_id)
        .one(db)
        .await
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "row vanished before activation".to_owned())?;
    activate_without_llm_review(db, row, definition).await
}

async fn activate_without_llm_review(
    db: &sea_orm::DatabaseConnection,
    row: tool::entity::tool::Model,
    definition: &SystemTool,
) -> Result<(), String> {
    let mut active: tool::entity::tool::ActiveModel = row.into();
    active.status = Set(Status::Active);
    active.name = Set(definition.name.to_owned());
    active.description = Set(definition.description.to_owned());
    active.input_schema = Set(definition.input_schema.clone());
    active.update(db).await.map_err(|error| error.to_string())?;
    Ok(())
}

async fn sync_schema(db: &sea_orm::DatabaseConnection) -> Result<(), Box<dyn std::error::Error>> {
    match db.get_schema_registry("tool::entity::*").sync(db).await {
        Ok(()) => Ok(()),
        Err(ref error) if is_missing_manual_unique_index(error) => {
            tracing::warn!(
                %error,
                "schema-sync tried to drop {MANUAL_UNIQUE_INDEX_NAME} with DROP CONSTRAINT, which \
                 Postgres refuses because it is a plain unique index and not constraint-owned; \
                 the index is untouched and INDEX_STATEMENTS_CREATED_AFTER_SCHEMA_SYNC re-asserts \
                 it, so this is ignored instead of crashing every restart"
            );
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

const POSTGRES_UNDEFINED_OBJECT_SQLSTATE: &str = "42704";

fn is_missing_manual_unique_index(error: &sea_orm::DbErr) -> bool {
    let (sea_orm::DbErr::Exec(sea_orm::RuntimeErr::SqlxError(sqlx_error))
    | sea_orm::DbErr::Query(sea_orm::RuntimeErr::SqlxError(sqlx_error))) = error
    else {
        return false;
    };
    let Some(db_error) = sqlx_error.as_database_error() else {
        return false;
    };
    db_error.code().as_deref() == Some(POSTGRES_UNDEFINED_OBJECT_SQLSTATE)
        && db_error.message().contains(MANUAL_UNIQUE_INDEX_NAME)
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
        std::env::var("YOU_SEARCH_API_KEY").unwrap_or_default(),
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
