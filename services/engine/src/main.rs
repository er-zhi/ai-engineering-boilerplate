// The engine binary: schema, built-in graphs, background loops, Connect RPC.

use std::sync::Arc;

use axum::routing::get;
use buffa::EnumValue;
use common::principal;
use common::proto::engine::v1::{
    CancelRequest, CancelResponse, CreateScheduleRequest, CreateScheduleResponse, EngineService,
    Execution as ExecutionProto, GetExecutionRequest, InterruptRequest, InterruptResponse,
    ListSchedulesRequest, ListSchedulesResponse, RegisterGraphRequest, RegisterGraphResponse,
    ResumeRequest, ResumeResponse, Schedule as ScheduleProto, StartExecutionRequest,
    StartExecutionResponse, StreamEventsRequest,
};
use connectrpc::{
    ConnectError, RequestContext, Response, Router as ConnectRouter, ServiceRequest, ServiceResult,
    ServiceStream,
};
use engine::executors::llm::LlmTaskExecutor;
use engine::executors::tool::ToolTaskExecutor;
use engine::service::Service;
use engine::tick::Tick;
use engine::{
    builtin_graphs, dispatch, entity, execution_event, partition, scheduler, stream, wakeup, wire,
};
use sea_orm::{ConnectionTrait, Database};
use uuid::Uuid;

const ENGINE_PORT: &str = "0.0.0.0:8085";
const LEASE_CONCURRENCY: usize = 8;
const DEFAULT_TERMINAL_RETENTION: std::time::Duration = std::time::Duration::from_secs(3600);
const SCHEDULER_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15);
const SECOND_ENGINE_REFUSED: &str =
    "another engine already holds the sole-engine lock: engine runs as a single replica";

fn env(name: &str) -> Result<String, String> {
    std::env::var(name).map_err(|_| format!("{name} is not set"))
}

fn terminal_retention() -> std::time::Duration {
    match std::env::var("ENGINE_TERMINAL_RETENTION") {
        Err(_) => DEFAULT_TERMINAL_RETENTION,
        Ok(value) => match value.parse::<u64>() {
            Ok(seconds) => std::time::Duration::from_secs(seconds),
            Err(_) => {
                tracing::warn!(
                    value,
                    "ENGINE_TERMINAL_RETENTION is not a whole number of seconds, using the default"
                );
                DEFAULT_TERMINAL_RETENTION
            }
        },
    }
}

fn events_retention_months() -> Option<u32> {
    let value = std::env::var("ENGINE_EVENTS_RETENTION_MONTHS").ok()?;
    if value.trim().is_empty() {
        return None;
    }
    match value.parse::<u32>() {
        Ok(months) if months > 0 => Some(months),
        _ => {
            tracing::warn!(
                value,
                "ENGINE_EVENTS_RETENTION_MONTHS is not a positive number of months, keeping every partition"
            );
            None
        }
    }
}

struct EngineServiceImpl {
    service: Arc<Service>,
    db: sea_orm::DatabaseConnection,
    wakeups: stream::Wakeups,
}

fn parse_uuid(value: &str, field: &str) -> Result<Uuid, ConnectError> {
    Uuid::parse_str(value).map_err(|_| {
        ConnectError::invalid_argument(format!("{field} is not a valid uuid: {value}"))
    })
}

fn continued_execution_if_readable(execution_id: Option<&str>) -> Option<Uuid> {
    execution_id.and_then(|id| Uuid::parse_str(id).ok())
}

#[allow(refining_impl_trait)]
impl EngineService for EngineServiceImpl {
    async fn register_graph(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, RegisterGraphRequest>,
    ) -> ServiceResult<RegisterGraphResponse> {
        let msg = request.to_owned_message();
        let (graph_id, version) = self
            .service
            .register_graph(msg.graph_id, &msg.definition_json)
            .await?;
        Response::ok(RegisterGraphResponse {
            graph_id,
            version,
            ..Default::default()
        })
    }

    async fn start_execution(
        &self,
        ctx: RequestContext,
        request: ServiceRequest<'_, StartExecutionRequest>,
    ) -> ServiceResult<StartExecutionResponse> {
        let msg = request.to_owned_message();
        let user_id = principal::from_metadata(ctx.headers()).map(|p| p.user_id);
        let continues = continued_execution_if_readable(msg.continues_execution_id.as_deref());
        let execution_id = self
            .service
            .start_continuing(
                msg.graph_id,
                msg.version,
                &msg.input_json,
                user_id,
                continues,
            )
            .await?;
        Response::ok(StartExecutionResponse {
            execution_id: execution_id.to_string(),
            ..Default::default()
        })
    }

    async fn interrupt(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, InterruptRequest>,
    ) -> ServiceResult<InterruptResponse> {
        let msg = request.to_owned_message();
        let execution_id = parse_uuid(&msg.execution_id, "execution_id")?;
        self.service
            .interrupt(execution_id, &msg.input_json)
            .await?;
        Response::ok(InterruptResponse::default())
    }

    async fn resume(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, ResumeRequest>,
    ) -> ServiceResult<ResumeResponse> {
        let msg = request.to_owned_message();
        let execution_id = parse_uuid(&msg.execution_id, "execution_id")?;
        self.service.resume(execution_id, &msg.event_json).await?;
        Response::ok(ResumeResponse::default())
    }

    async fn cancel(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, CancelRequest>,
    ) -> ServiceResult<CancelResponse> {
        let msg = request.to_owned_message();
        let execution_id = parse_uuid(&msg.execution_id, "execution_id")?;
        self.service.cancel(execution_id).await?;
        Response::ok(CancelResponse::default())
    }

    async fn get_execution(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, GetExecutionRequest>,
    ) -> ServiceResult<ExecutionProto> {
        let msg = request.to_owned_message();
        let execution_id = parse_uuid(&msg.execution_id, "execution_id")?;
        let execution = self.service.get_execution(execution_id).await?;
        Response::ok(execution_to_proto(&execution))
    }

    async fn stream_events(
        &self,
        ctx: RequestContext,
        request: ServiceRequest<'_, StreamEventsRequest>,
    ) -> ServiceResult<ServiceStream<common::proto::engine::v1::ExecutionEvent>> {
        let msg = request.to_owned_message();
        let execution_id = msg
            .execution_id
            .as_deref()
            .map(|s| parse_uuid(s, "execution_id"))
            .transpose()?;
        if let Some(id) = execution_id {
            let owner = self.service.get_execution(id).await?.user_id.map(|u| u.0);
            if owner.is_some()
                && owner != principal::from_metadata(ctx.headers()).map(|p| p.user_id)
            {
                return Err(ConnectError::permission_denied(format!(
                    "execution {id} belongs to another user"
                )));
            }
        }
        let scope = match execution_id {
            Some(id) => stream::Scope::Execution(id),
            None => stream::Scope::User(
                principal::from_metadata(ctx.headers())
                    .map(|p| p.user_id)
                    .ok_or_else(|| {
                        ConnectError::invalid_argument(
                            "a user-scoped StreamEvents needs a Principal in the request metadata",
                        )
                    })?,
            ),
        };
        Response::stream_ok(stream::stream_events(
            self.db.clone(),
            scope,
            self.wakeups.clone(),
        ))
    }

    async fn create_schedule(
        &self,
        ctx: RequestContext,
        request: ServiceRequest<'_, CreateScheduleRequest>,
    ) -> ServiceResult<CreateScheduleResponse> {
        let msg = request.to_owned_message();
        let user_id = principal::from_metadata(ctx.headers()).map(|p| p.user_id);
        let schedule_id = self
            .service
            .create_schedule(
                msg.graph_id,
                msg.version,
                &msg.cron_expr,
                &msg.input_json,
                user_id,
            )
            .await?;
        Response::ok(CreateScheduleResponse {
            schedule_id: schedule_id.to_string(),
            ..Default::default()
        })
    }

    async fn list_schedules(
        &self,
        ctx: RequestContext,
        _request: ServiceRequest<'_, ListSchedulesRequest>,
    ) -> ServiceResult<ListSchedulesResponse> {
        let user_id = principal::from_metadata(ctx.headers()).map(|p| p.user_id);
        let schedules = self.service.list_schedules(user_id).await?;
        Response::ok(ListSchedulesResponse {
            schedules: schedules.iter().map(schedule_to_proto).collect(),
            ..Default::default()
        })
    }
}

fn schedule_to_proto(row: &engine::entity::schedule::Model) -> ScheduleProto {
    ScheduleProto {
        id: row.id.to_string(),
        graph_id: row.graph_id.clone(),
        cron_expr: row.cron_expr.clone(),
        enabled: row.enabled,
        next_run_at: row.next_run_at.to_rfc3339(),
        last_execution_id: row.last_execution_id.map(|id| id.to_string()),
        ..Default::default()
    }
}

fn execution_to_proto(execution: &engine_core::Execution) -> ExecutionProto {
    ExecutionProto {
        execution_id: execution.id.to_string(),
        graph_id: execution.graph_id.to_string(),
        graph_version: i32::try_from(execution.graph_version).unwrap_or(i32::MAX),
        user_id: execution.user_id.map(|u| u.0.to_string()),
        status: EnumValue::Known(wire::status_to_proto(&execution.status)),
        ..Default::default()
    }
}

async fn prepare_schema(
    db: &sea_orm::DatabaseConnection,
) -> Result<(), Box<dyn std::error::Error>> {
    db.get_schema_registry("engine::entity::*").sync(db).await?;
    for statement in execution_event::TABLE_STATEMENTS
        .iter()
        .chain(entity::execution::INDEX_STATEMENTS_CREATED_AFTER_SCHEMA_SYNC.iter())
        .chain(entity::schedule::INDEX_STATEMENTS_CREATED_AFTER_SCHEMA_SYNC.iter())
        .chain(execution_event::INDEX_STATEMENTS_CREATED_AFTER_SCHEMA_SYNC.iter())
    {
        db.execute_unprepared(statement).await?;
    }
    if !partition::is_partitioned(db).await? {
        tracing::error!(
            "engine.execution_events exists as a plain table, so the partitioned CREATE TABLE was \
             a no-op: the event log will grow without a DROP PARTITION retention path. Drop the \
             table (its contents are reproducible history, not working state) and restart."
        );
    }
    partition::maintain(db, chrono::Utc::now(), events_retention_months()).await?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    common::logging::init();

    let database_url = env("DATABASE_URL")?;
    let db = Database::connect(&database_url).await?;
    prepare_schema(&db).await?;

    let Some(_sole_engine) = engine::lease::claim_sole_engine_lock(&db).await? else {
        return Err(SECOND_ENGINE_REFUSED.into());
    };
    let llm_router_url = env("LLM_ROUTER_URL")?;
    let tool_service_url = env("TOOL_SERVICE_URL")?;
    let executor = dispatch::Dispatcher::new(
        LlmTaskExecutor::new(&llm_router_url, &tool_service_url)?,
        ToolTaskExecutor::new(&tool_service_url)?,
    );
    match engine::lease::expire_abandoned_leases(&db).await {
        Ok(0) => {}
        Ok(freed) => tracing::info!(
            freed,
            "recovered executions left running by a previous engine"
        ),
        Err(error) => tracing::error!(%error, "could not recover abandoned leases at startup"),
    }

    let owner = format!("engine-{}", Uuid::new_v4());
    let tick = Arc::new(Tick::new(db.clone(), executor, owner));
    let wakeup_database_url = database_url.clone();
    let maintenance = wakeup::Maintenance {
        db: db.clone(),
        terminal_retention: terminal_retention(),
        events_retention_months: events_retention_months(),
    };
    tokio::spawn(async move {
        wakeup::run_forever(tick, &wakeup_database_url, LEASE_CONCURRENCY, maintenance).await;
    });

    let service = Arc::new(Service::new(db.clone()));
    builtin_graphs::register_all(&service).await;

    let scheduler_db = db.clone();
    let scheduler_service = Arc::clone(&service);
    tokio::spawn(async move {
        scheduler::run_forever(scheduler_db, scheduler_service, SCHEDULER_POLL_INTERVAL).await;
    });

    let wakeups = stream::Wakeups::listening(&database_url).await;
    let engine_service = EngineServiceImpl {
        service,
        db,
        wakeups,
    };
    let connect = ConnectRouter::new().add_service(Arc::new(engine_service));
    let app = axum::Router::new()
        .route("/health", get(|| async { "OK" }))
        .fallback_service(connect.into_axum_service());

    let listener = tokio::net::TcpListener::bind(ENGINE_PORT).await?;
    tracing::info!("engine listening on {ENGINE_PORT}");
    axum::serve(listener, app).await?;
    Ok(())
}
