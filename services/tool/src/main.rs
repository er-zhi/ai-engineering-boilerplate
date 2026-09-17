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

/// One seedable system tool. The schemas matter: `list_tools` hands `input_schema` straight to
/// the agent's prompt, so this is the only place that tells the model what arguments
/// `weather`/`fx_rate`/`stock_quote` take.
struct SystemTool {
    slug: &'static str,
    name: &'static str,
    description: &'static str,
    risk: Risk,
    timeout_seconds: i32,
    input_schema: serde_json::Value,
    output_schema: serde_json::Value,
}

fn system_tools() -> Vec<SystemTool> {
    let object = || serde_json::json!({"type": "object"});
    let mut tools = vec![
        SystemTool {
            slug: slugs::WEB_SEARCH,
            name: "Web search",
            description: "Searches the web via Brave Search and returns matching pages.",
            risk: Risk::ReadOnly,
            timeout_seconds: 30,
            input_schema: object(),
            output_schema: object(),
        },
        SystemTool {
            slug: slugs::WEB_FETCH,
            name: "Fetch a web page",
            description: "Fetches a URL and returns its readable title and text.",
            risk: Risk::ReadOnly,
            timeout_seconds: 30,
            input_schema: object(),
            output_schema: object(),
        },
        SystemTool {
            slug: slugs::KB_SEARCH,
            name: "Knowledge base search",
            description: "Searches this project's knowledge base.",
            risk: Risk::ReadOnly,
            timeout_seconds: 30,
            input_schema: object(),
            output_schema: object(),
        },
        SystemTool {
            slug: slugs::KB_READ_DOCUMENT,
            name: "Read a knowledge base document",
            description: "Reads the full text of one knowledge base document.",
            risk: Risk::ReadOnly,
            timeout_seconds: 30,
            input_schema: object(),
            output_schema: object(),
        },
    ];
    tools.extend(structured_data_tools());
    tools
}

/// The three structured-data tools (Task: live weather/FX/market data). Each races several open,
/// keyless APIs internally, so from the agent's side they are a single fast call that either
/// returns the real numbers or an error — which is why the prompt prefers them over web_search.
fn structured_data_tools() -> Vec<SystemTool> {
    vec![
        weather_system_tool(),
        fx_system_tool(),
        stock_quote_system_tool(),
    ]
}

fn weather_system_tool() -> SystemTool {
    SystemTool {
        slug: slugs::WEATHER,
        name: "Current weather",
        description: "Current weather for a place, from live weather APIs. Use this for any weather question instead of searching the web. Input: {\"location\": \"San Francisco\"} (a city, \"city, country\", or postcode).",
        risk: Risk::ReadOnly,
        timeout_seconds: 15,
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "location": {
                    "type": "string",
                    "description": "City, \"city, country\", or postcode, e.g. \"San Francisco\" or \"Berlin, DE\"."
                }
            },
            "required": ["location"],
        }),
        output_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "location": {"type": "string"},
                "temp_c": {"type": "number"},
                "temp_f": {"type": "number"},
                "conditions": {"type": "string"},
                "humidity": {"type": ["number", "null"]},
                "wind_kmh": {"type": ["number", "null"]},
                "source": {"type": "string"},
                "observed_at": {"type": "string"},
            },
        }),
    }
}

fn fx_system_tool() -> SystemTool {
    SystemTool {
        slug: slugs::FX_RATE,
        name: "Currency exchange rate",
        description: "Current exchange rate between two currencies, from live FX APIs. Use this for any currency or conversion question instead of searching the web. Input: {\"base\": \"USD\", \"quote\": \"EUR\", \"amount\": 100} — amount is optional and returns the converted value.",
        risk: Risk::ReadOnly,
        timeout_seconds: 15,
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "base": {"type": "string", "description": "ISO 4217 code to convert FROM, e.g. \"USD\"."},
                "quote": {"type": "string", "description": "ISO 4217 code to convert TO, e.g. \"EUR\"."},
                "amount": {"type": "number", "description": "Optional amount of the base currency to convert."},
            },
            "required": ["base", "quote"],
        }),
        output_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "base": {"type": "string"},
                "quote": {"type": "string"},
                "rate": {"type": "number"},
                "amount": {"type": ["number", "null"]},
                "converted": {"type": ["number", "null"]},
                "date": {"type": "string"},
                "source": {"type": "string"},
            },
        }),
    }
}

fn stock_quote_system_tool() -> SystemTool {
    SystemTool {
        slug: slugs::STOCK_QUOTE,
        name: "Stock or index quote",
        description: "Latest price of a stock or market index, from live market-data APIs. Use this for any share price or index level question instead of searching the web. Input: {\"symbol\": \"AAPL\"}; indices use their Yahoo symbols: ^IXIC (Nasdaq Composite), ^GSPC (S&P 500), ^DJI (Dow Jones).",
        risk: Risk::ReadOnly,
        timeout_seconds: 15,
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "symbol": {
                    "type": "string",
                    "description": "Ticker or index symbol, e.g. \"AAPL\", \"MSFT\", \"^IXIC\", \"^GSPC\", \"^DJI\"."
                }
            },
            "required": ["symbol"],
        }),
        output_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "symbol": {"type": "string"},
                "name": {"type": ["string", "null"]},
                "price": {"type": "number"},
                "previous_close": {"type": ["number", "null"]},
                "change_pct": {"type": ["number", "null"]},
                "currency": {"type": ["string", "null"]},
                "as_of": {"type": "string"},
                "source": {"type": "string"},
            },
        }),
    }
}

async fn seed_system_tools(service: &Service, db: &sea_orm::DatabaseConnection) {
    for tool in system_tools() {
        let existing = Entity::find()
            .filter(tool::entity::tool::Column::UserId.is_null())
            .filter(tool::entity::tool::Column::Slug.eq(tool.slug))
            .one(db)
            .await
            .unwrap_or(None);
        // A row already at Active needs nothing further. One still stuck below Active — e.g.
        // a prior boot's create succeeded but the process died before the activation update
        // ran — is retried rather than skipped forever, so a one-time partial failure doesn't
        // become a permanent one.
        if existing
            .as_ref()
            .is_some_and(|row| row.status == Status::Active)
        {
            continue;
        }
        seed_one_system_tool(service, db, &tool, existing).await;
    }
}

/// System tools are pre-vetted Rust implementations, not user-submitted schemas — they skip the
/// Draft -> Validated LLM-review step and go straight to Active.
async fn seed_one_system_tool(
    service: &Service,
    db: &sea_orm::DatabaseConnection,
    tool: &SystemTool,
    existing: Option<tool::entity::tool::Model>,
) {
    let result = match existing {
        Some(row) => activate_system_tool(db, row).await,
        None => create_and_activate_system_tool(service, db, tool).await,
    };
    match result {
        Ok(()) => tracing::info!(slug = tool.slug, "seeded system tool"),
        Err(error) => tracing::error!(slug = tool.slug, %error, "failed to seed system tool"),
    }
}

async fn create_and_activate_system_tool(
    service: &Service,
    db: &sea_orm::DatabaseConnection,
    tool: &SystemTool,
) -> Result<(), String> {
    let tool_id = service
        .create_tool(
            None,
            tool.slug.to_owned(),
            tool.name.to_owned(),
            tool.description.to_owned(),
            tool.input_schema.clone(),
            tool.output_schema.clone(),
            tool.risk,
            tool.timeout_seconds,
        )
        .await
        .map_err(|error| error.to_string())?;
    let row = Entity::find_by_id(tool_id)
        .one(db)
        .await
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "row vanished before activation".to_owned())?;
    activate_system_tool(db, row).await
}

async fn activate_system_tool(
    db: &sea_orm::DatabaseConnection,
    row: tool::entity::tool::Model,
) -> Result<(), String> {
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
        Err(ref error) if is_known_drop_constraint_error(error) => {
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

/// True only for Postgres SQLSTATE 42704 ("undefined object") against `tools_owner_slug_idx`
/// specifically — narrower than a message substring, so a real unique-violation or permissions
/// error that happens to mention the index name is never mistaken for the known, harmless
/// DROP-CONSTRAINT-on-a-plain-index failure `sync_schema` swallows.
fn is_known_drop_constraint_error(error: &sea_orm::DbErr) -> bool {
    let (sea_orm::DbErr::Exec(sea_orm::RuntimeErr::SqlxError(sqlx_error))
    | sea_orm::DbErr::Query(sea_orm::RuntimeErr::SqlxError(sqlx_error))) = error
    else {
        return false;
    };
    let Some(db_error) = sqlx_error.as_database_error() else {
        return false;
    };
    db_error.code().as_deref() == Some("42704")
        && db_error.message().contains("tools_owner_slug_idx")
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
