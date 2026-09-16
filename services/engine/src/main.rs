// engine: the stateless graph-execution service. Connects to Postgres, syncs its schema,
// registers the built-in graphs (simple/rag/agent) if they aren't already there, starts the
// tick loop's wakeup task in the background, and serves Connect RPC. See the spec's "Runtime
// сервиса engine" for what the background task actually does.

// `engine` is a lib+bin crate (since Task 11): this binary links against the library target of
// the same package rather than re-declaring `mod` lines for entity/service/etc — a `mod` line
// here would compile a second, separate copy of every module (its own `test_db`, unused by the
// binary, denied as dead code under `-D warnings`) instead of reusing the one `cargo test`
// already exercises.

use std::sync::Arc;

use axum::routing::get;
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
use engine::service::Service;
use engine::tick::Tick;
use engine::{
    dispatch, entity, execution_event, partition, principal, scheduler, stream, wakeup, wire,
};
use sea_orm::{ConnectionTrait, Database};
use uuid::Uuid;

const ENGINE_PORT: &str = "0.0.0.0:8085";
const LEASE_CONCURRENCY: usize = 8;
/// How long a terminal execution's row and checkpoints survive before `sweep` takes them: long
/// enough that a client which just called `StartExecution` still finds its result with
/// `GetExecution`, after which `StreamEvents` is the contract for history.
const DEFAULT_TERMINAL_RETENTION: std::time::Duration = std::time::Duration::from_secs(3600);
// Schedule firing is not latency-sensitive the way a chat reply is: a cron run a few seconds late
// is invisible, so a plain interval replaces the tick loop's LISTEN/NOTIFY here.
const SCHEDULER_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15);

fn env(name: &str) -> Result<String, String> {
    std::env::var(name).map_err(|_| format!("{name} is not set"))
}

/// `ENGINE_TERMINAL_RETENTION`, in seconds. Unset or unreadable falls back to the default rather
/// than refusing to start: a misspelt retention must not take the service down.
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

/// `ENGINE_EVENTS_RETENTION_MONTHS`. Unset — the default — keeps every partition forever: the
/// event log is the durable history everything else in this service is allowed to be swept
/// against, so dropping from it is an explicit decision, never one made by omission.
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
    // Arc, not a plain Service: the scheduler task started in main() drives the very same
    // Service (its start_execution is what a due schedule fires), so both hold one instance.
    service: Arc<Service>,
    db: sea_orm::DatabaseConnection,
}

fn parse_uuid(value: &str, field: &str) -> Result<Uuid, ConnectError> {
    Uuid::parse_str(value).map_err(|_| {
        ConnectError::invalid_argument(format!("{field} is not a valid uuid: {value}"))
    })
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
        let execution_id = self
            .service
            .start_execution(msg.graph_id, msg.version, &msg.input_json, user_id)
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
        self.service
            .resume(execution_id, &msg.wait_key, &msg.event_json)
            .await?;
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
        // A user-scoped stream is authorization-sensitive: the body's `user_id` is whatever the
        // caller typed, so it's ignored entirely and the filter comes from the Principal headers
        // Gateway stamps (same source start_execution already uses). Without a Principal there
        // is no user to scope to — reject rather than fall back to the body's value.
        // Execution-scoped streams carry the same cross-user risk when the execution belongs to
        // somebody: an execution with a `user_id` is only streamable by that user. Executions
        // with no `user_id` (nothing fronts Engine with Principal headers yet) stay open, so
        // this closes the cross-user hole without breaking the unauthenticated path.
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
        let user_id = if execution_id.is_some() {
            None
        } else {
            Some(
                principal::from_metadata(ctx.headers())
                    .map(|p| p.user_id)
                    .ok_or_else(|| {
                        ConnectError::invalid_argument(
                            "a user-scoped StreamEvents needs a Principal in the request metadata",
                        )
                    })?,
            )
        };
        Response::stream_ok(stream::stream_events(
            self.db.clone(),
            execution_id,
            user_id,
        ))
    }

    async fn create_schedule(
        &self,
        ctx: RequestContext,
        request: ServiceRequest<'_, CreateScheduleRequest>,
    ) -> ServiceResult<CreateScheduleResponse> {
        let msg = request.to_owned_message();
        // Ownership comes from the Principal headers Gateway stamps, never from the body — the
        // same rule start_execution and stream_events already follow.
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
    let (status, wait_kind_json) = wire::status_to_columns(&execution.status);
    ExecutionProto {
        execution_id: execution.id.to_string(),
        graph_id: execution.graph_id.to_string(),
        graph_version: i32::try_from(execution.graph_version).unwrap_or(i32::MAX),
        user_id: execution.user_id.map(|u| u.0.to_string()),
        status,
        wait_kind_json: wait_kind_json.map(|v| v.to_string()),
        current_nodes_json: serde_json::to_string(&execution.current_nodes).unwrap_or_default(),
        iteration: i32::try_from(execution.iteration).unwrap_or(i32::MAX),
        state_json: execution.state.to_string(),
        ..Default::default()
    }
}

async fn register_builtin_graphs(service: &Service) {
    for (name, definition) in [
        ("simple", engine_core::simple_graph()),
        ("rag", engine_core::rag_graph()),
        ("agent", engine_core::agent_graph()),
    ] {
        let definition_json =
            serde_json::to_string(&definition).expect("built-in graphs always serialize");
        match service
            .register_graph(name.to_owned(), &definition_json)
            .await
        {
            Ok((_, version)) => tracing::info!(graph = name, version, "registered built-in graph"),
            Err(error) => {
                tracing::error!(graph = name, %error, "failed to register built-in graph")
            }
        }
    }
}

/// Schema-sync for the four entities under `engine::entity::*`, then the pieces it cannot express:
/// the partitioned `execution_events` parent, the two startup indexes, and this month's and next
/// month's event partitions. All `IF NOT EXISTS` — every start after the first is a no-op.
async fn prepare_schema(
    db: &sea_orm::DatabaseConnection,
) -> Result<(), Box<dyn std::error::Error>> {
    db.get_schema_registry("engine::entity::*").sync(db).await?;
    for statement in execution_event::TABLE_STATEMENTS
        .iter()
        .chain(entity::execution::INDEX_STATEMENTS_CREATED_AFTER_SCHEMA_SYNC.iter())
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

    let executor = dispatch::Dispatcher {
        llm: LlmTaskExecutor::new(&env("LLM_ROUTER_URL")?)?,
    };
    let owner = format!("engine-{}", Uuid::new_v4());
    let tick = Arc::new(Tick::new(db.clone(), executor, owner));
    // wakeup::run_forever takes &str (Task 16); tokio::spawn needs a 'static future, so the URL
    // is moved into the spawned block as an owned String and borrowed only from within it — a
    // bare `&database_url` here would borrow a local that doesn't outlive the spawn.
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
    register_builtin_graphs(&service).await;

    // The cron half of the runtime, alongside the tick loop above: one due schedule per poll,
    // starting ordinary executions through the same Service the RPC handlers use.
    let scheduler_db = db.clone();
    let scheduler_service = Arc::clone(&service);
    tokio::spawn(async move {
        scheduler::run_forever(scheduler_db, scheduler_service, SCHEDULER_POLL_INTERVAL).await;
    });

    let engine_service = EngineServiceImpl { service, db };
    let connect = ConnectRouter::new().add_service(Arc::new(engine_service));
    let app = axum::Router::new()
        .route("/health", get(|| async { "OK" }))
        .fallback_service(connect.into_axum_service());

    let listener = tokio::net::TcpListener::bind(ENGINE_PORT).await?;
    tracing::info!("engine listening on {ENGINE_PORT}");
    axum::serve(listener, app).await?;
    Ok(())
}
