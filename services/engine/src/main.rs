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
    CancelRequest, CancelResponse, EngineService, Execution as ExecutionProto, GetExecutionRequest,
    InterruptRequest, InterruptResponse, RegisterGraphRequest, RegisterGraphResponse,
    ResumeRequest, ResumeResponse, StartExecutionRequest, StartExecutionResponse,
    StreamEventsRequest,
};
use connectrpc::{
    ConnectError, RequestContext, Response, Router as ConnectRouter, ServiceRequest, ServiceResult,
    ServiceStream,
};
use engine::entity::execution_event::INDEX_STATEMENTS_CREATED_AFTER_SCHEMA_SYNC;
use engine::executors::llm::LlmTaskExecutor;
use engine::service::Service;
use engine::tick::Tick;
use engine::{dispatch, principal, stream, wakeup, wire};
use sea_orm::{ConnectionTrait, Database};
use uuid::Uuid;

const ENGINE_PORT: &str = "0.0.0.0:8085";
const LEASE_CONCURRENCY: usize = 8;

fn env(name: &str) -> Result<String, String> {
    std::env::var(name).map_err(|_| format!("{name} is not set"))
}

struct EngineServiceImpl {
    service: Service,
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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    common::logging::init();

    let database_url = env("DATABASE_URL")?;
    let db = Database::connect(&database_url).await?;
    db.get_schema_registry("engine::entity::*")
        .sync(&db)
        .await?;
    for statement in INDEX_STATEMENTS_CREATED_AFTER_SCHEMA_SYNC {
        db.execute_unprepared(statement).await?;
    }

    let executor = dispatch::Dispatcher {
        llm: LlmTaskExecutor::new(&env("LLM_ROUTER_URL")?)?,
    };
    let owner = format!("engine-{}", Uuid::new_v4());
    let tick = Arc::new(Tick::new(db.clone(), executor, owner));
    // wakeup::run_forever takes &str (Task 16); tokio::spawn needs a 'static future, so the URL
    // is moved into the spawned block as an owned String and borrowed only from within it — a
    // bare `&database_url` here would borrow a local that doesn't outlive the spawn.
    let wakeup_database_url = database_url.clone();
    tokio::spawn(async move {
        wakeup::run_forever(tick, &wakeup_database_url, LEASE_CONCURRENCY).await;
    });

    let service = Service::new(db.clone());
    register_builtin_graphs(&service).await;

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
