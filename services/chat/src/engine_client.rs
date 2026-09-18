// The only place Chat talks to engine.v1.EngineService.

use chrono::{DateTime, Utc};
use common::principal::{Principal, SESSION_ID_HEADER, USER_ID_HEADER};
use common::proto::engine::v1::{
    EngineServiceClient, ExecutionEvent, ExecutionEventKind, InterruptRequest,
    StartExecutionRequest, StreamEventsRequest,
};
use connectrpc::client::{CallOptions, ClientConfig, HttpClient};
use connectrpc::{ConnectError, Protocol};
use std::time::Duration;
use tokio::sync::mpsc;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineEvent {
    pub id: String,
    pub kind: ExecutionEventKind,
    pub payload_json: String,
    pub result: Option<String>,
    pub error: Option<String>,
    pub occurred_at: DateTime<Utc>,
}

impl From<ExecutionEvent> for EngineEvent {
    fn from(event: ExecutionEvent) -> Self {
        Self {
            id: event.id,
            kind: event.payload_kind.as_known().unwrap_or_default(),
            payload_json: event.payload_json,
            result: event.result,
            error: event.error,
            occurred_at: DateTime::parse_from_rfc3339(&event.occurred_at)
                .map(|at| at.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now()),
        }
    }
}

fn principal_options(principal: &Principal) -> CallOptions {
    CallOptions::default()
        .with_header(USER_ID_HEADER, principal.user_id.to_string())
        .with_header(SESSION_ID_HEADER, principal.session_id.as_str())
}

const CALL_TIMEOUT: Duration = Duration::from_secs(20);
const LONGEST_WATCHABLE_EXECUTION: Duration = Duration::from_secs(3_600);

pub struct EngineClient {
    inner: EngineServiceClient<HttpClient>,
    stream_timeout: Duration,
}

impl EngineClient {
    pub fn new(engine_url: &str) -> Result<Self, String> {
        Self::with_timeouts(engine_url, CALL_TIMEOUT, LONGEST_WATCHABLE_EXECUTION)
    }

    pub fn with_timeouts(
        engine_url: &str,
        call_timeout: Duration,
        stream_timeout: Duration,
    ) -> Result<Self, String> {
        let target = engine_url
            .parse()
            .map_err(|e| format!("could not parse ENGINE_URL {engine_url:?}: {e}"))?;
        Ok(Self {
            inner: EngineServiceClient::new(
                HttpClient::plaintext_http2_only(),
                ClientConfig::new(target)
                    .with_protocol(Protocol::Grpc)
                    .with_default_timeout(call_timeout)
                    .proto(),
            ),
            stream_timeout,
        })
    }

    pub async fn start_execution(
        &self,
        principal: &Principal,
        graph_id: &str,
        input_json: &str,
    ) -> Result<Uuid, String> {
        let response = self
            .inner
            .start_execution_with_options(
                StartExecutionRequest {
                    graph_id: graph_id.to_owned(),
                    input_json: input_json.to_owned(),
                    ..Default::default()
                },
                principal_options(principal),
            )
            .await
            .map_err(|e| e.to_string())?
            .into_owned();
        Uuid::parse_str(&response.execution_id).map_err(|e| e.to_string())
    }

    /// Kept as the raw `ConnectError`, not stringified like this client's other calls: its `code`
    /// is what lets `topic_turn.rs` tell an execution that is merely busy (`unavailable` — see
    /// `services/engine/src/error.rs`'s `EngineError::Busy`) apart from one Engine genuinely could
    /// not interrupt, which callers must still treat as a real failure.
    pub async fn interrupt(
        &self,
        principal: &Principal,
        execution_id: Uuid,
        input_json: &str,
    ) -> Result<(), ConnectError> {
        self.inner
            .interrupt_with_options(
                InterruptRequest {
                    execution_id: execution_id.to_string(),
                    input_json: input_json.to_owned(),
                    ..Default::default()
                },
                principal_options(principal),
            )
            .await?;
        Ok(())
    }

    pub async fn stream_events(
        &self,
        principal: &Principal,
        execution_id: Uuid,
    ) -> Result<mpsc::UnboundedReceiver<EngineEvent>, String> {
        let mut stream = self
            .inner
            .stream_events_with_options(
                StreamEventsRequest {
                    execution_id: Some(execution_id.to_string()),
                    user_id: None,
                    ..Default::default()
                },
                principal_options(principal).with_timeout(self.stream_timeout),
            )
            .await
            .map_err(|e| e.to_string())?;
        let (sender, receiver) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            loop {
                match stream.message::<ExecutionEvent>().await {
                    Ok(Some(item)) => {
                        if sender
                            .send(EngineEvent::from(item.to_owned_message()))
                            .is_err()
                        {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(error) => {
                        tracing::error!(%error, "engine event stream error");
                        break;
                    }
                }
            }
        });
        Ok(receiver)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::proto::engine::v1::{
        CancelRequest, CancelResponse, CreateScheduleRequest, CreateScheduleResponse,
        EngineService, Execution, GetExecutionRequest, InterruptResponse, ListSchedulesRequest,
        ListSchedulesResponse, RegisterGraphRequest, RegisterGraphResponse, ResumeRequest,
        ResumeResponse, StartExecutionResponse,
    };
    use connectrpc::{
        RequestContext, Response, Router as ConnectRouter, ServiceRequest, ServiceResult,
        ServiceStream,
    };
    use std::sync::{Arc, Mutex};

    struct FakeEngine {
        execution_id: String,
        received_interrupts: Mutex<Vec<InterruptRequest>>,
        received_principals: Mutex<Vec<Option<Principal>>>,
        stream_delay: Duration,
        interrupt_error: Mutex<Option<connectrpc::ErrorCode>>,
    }

    fn fake_engine(execution_id: Uuid) -> Arc<FakeEngine> {
        fake_engine_delayed(execution_id, Duration::ZERO)
    }

    fn fake_engine_delayed(execution_id: Uuid, stream_delay: Duration) -> Arc<FakeEngine> {
        Arc::new(FakeEngine {
            execution_id: execution_id.to_string(),
            received_interrupts: Mutex::new(Vec::new()),
            received_principals: Mutex::new(Vec::new()),
            stream_delay,
            interrupt_error: Mutex::new(None),
        })
    }

    impl FakeEngine {
        fn record(&self, ctx: &RequestContext) {
            self.received_principals
                .lock()
                .expect("lock")
                .push(common::principal::from_metadata(ctx.headers()));
        }
    }

    fn a_principal() -> Principal {
        Principal {
            user_id: Uuid::new_v4(),
            session_id: Uuid::new_v4().to_string(),
        }
    }

    #[allow(refining_impl_trait)]
    impl EngineService for FakeEngine {
        async fn register_graph(
            &self,
            _ctx: RequestContext,
            _request: ServiceRequest<'_, RegisterGraphRequest>,
        ) -> ServiceResult<RegisterGraphResponse> {
            Response::ok(RegisterGraphResponse::default())
        }
        async fn start_execution(
            &self,
            ctx: RequestContext,
            _request: ServiceRequest<'_, StartExecutionRequest>,
        ) -> ServiceResult<StartExecutionResponse> {
            self.record(&ctx);
            Response::ok(StartExecutionResponse {
                execution_id: self.execution_id.clone(),
                ..Default::default()
            })
        }
        async fn interrupt(
            &self,
            ctx: RequestContext,
            request: ServiceRequest<'_, InterruptRequest>,
        ) -> ServiceResult<InterruptResponse> {
            self.record(&ctx);
            self.received_interrupts
                .lock()
                .expect("lock")
                .push(request.to_owned_message());
            if let Some(code) = *self.interrupt_error.lock().expect("lock") {
                return Err(connectrpc::ConnectError::new(code, "configured failure"));
            }
            Response::ok(InterruptResponse::default())
        }
        async fn resume(
            &self,
            _ctx: RequestContext,
            _request: ServiceRequest<'_, ResumeRequest>,
        ) -> ServiceResult<ResumeResponse> {
            Response::ok(ResumeResponse::default())
        }
        async fn cancel(
            &self,
            _ctx: RequestContext,
            _request: ServiceRequest<'_, CancelRequest>,
        ) -> ServiceResult<CancelResponse> {
            Response::ok(CancelResponse::default())
        }
        async fn get_execution(
            &self,
            _ctx: RequestContext,
            _request: ServiceRequest<'_, GetExecutionRequest>,
        ) -> ServiceResult<Execution> {
            Response::ok(Execution::default())
        }
        async fn stream_events(
            &self,
            ctx: RequestContext,
            _request: ServiceRequest<'_, StreamEventsRequest>,
        ) -> ServiceResult<ServiceStream<ExecutionEvent>> {
            self.record(&ctx);
            let event = ExecutionEvent {
                id: "e1".to_owned(),
                execution_id: self.execution_id.clone(),
                payload_kind: ExecutionEventKind::ExecutionCompleted.into(),
                payload_json: r#"{"final_state":{},"result":"done"}"#.to_owned(),
                result: Some("done".to_owned()),
                ..Default::default()
            };
            let delay = self.stream_delay;
            Response::stream_ok(futures::stream::once(async move {
                tokio::time::sleep(delay).await;
                Ok(event)
            }))
        }
        async fn create_schedule(
            &self,
            _ctx: RequestContext,
            _request: ServiceRequest<'_, CreateScheduleRequest>,
        ) -> ServiceResult<CreateScheduleResponse> {
            Response::ok(CreateScheduleResponse::default())
        }
        async fn list_schedules(
            &self,
            _ctx: RequestContext,
            _request: ServiceRequest<'_, ListSchedulesRequest>,
        ) -> ServiceResult<ListSchedulesResponse> {
            Response::ok(ListSchedulesResponse::default())
        }
    }

    async fn serve(fake: Arc<FakeEngine>) -> String {
        let connect = ConnectRouter::new().add_service(fake);
        let app = axum::Router::new().fallback_service(connect.into_axum_service());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("addr");
        tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
        format!("http://{address}")
    }

    #[tokio::test]
    async fn start_execution_returns_the_parsed_execution_id() {
        let execution_id = Uuid::new_v4();
        let fake = fake_engine(execution_id);
        let url = serve(fake).await;
        let client = EngineClient::new(&url).expect("client");

        let got = client
            .start_execution(&a_principal(), "agent", r#"{"question": "hi"}"#)
            .await
            .expect("start");

        assert_eq!(got, execution_id);
    }

    #[tokio::test]
    async fn interrupt_sends_the_execution_id_and_input() {
        let execution_id = Uuid::new_v4();
        let fake = fake_engine(execution_id);
        let url = serve(Arc::clone(&fake)).await;
        let client = EngineClient::new(&url).expect("client");

        client
            .interrupt(&a_principal(), execution_id, r#"{"question": "more"}"#)
            .await
            .expect("interrupt");

        let received = fake.received_interrupts.lock().expect("lock");
        assert_eq!(received[0].execution_id, execution_id.to_string());
        assert_eq!(received[0].input_json, r#"{"question": "more"}"#);
    }

    // `interrupt`'s Err carries the real `ConnectError`, not a stringified one, precisely so a
    // caller can tell `unavailable` (Engine merely busy) apart from anything else — see the doc on
    // `interrupt` above. This pins that the code survives the round trip over the wire.
    #[tokio::test]
    async fn interrupts_error_code_survives_the_round_trip() {
        let execution_id = Uuid::new_v4();
        let fake = fake_engine(execution_id);
        *fake.interrupt_error.lock().expect("lock") = Some(connectrpc::ErrorCode::Unavailable);
        let url = serve(Arc::clone(&fake)).await;
        let client = EngineClient::new(&url).expect("client");

        let error = client
            .interrupt(&a_principal(), execution_id, "{}")
            .await
            .expect_err("the fake is configured to fail this call");

        assert_eq!(error.code, connectrpc::ErrorCode::Unavailable);
    }

    #[tokio::test]
    async fn stream_events_yields_the_engine_stream() {
        let execution_id = Uuid::new_v4();
        let fake = fake_engine(execution_id);
        let url = serve(fake).await;
        let client = EngineClient::new(&url).expect("client");

        let mut events = client
            .stream_events(&a_principal(), execution_id)
            .await
            .expect("stream");
        let event = events.recv().await.expect("one event");

        assert_eq!(event.kind, ExecutionEventKind::ExecutionCompleted);
        assert!(
            events.recv().await.is_none(),
            "stream ends cleanly after one event"
        );
    }

    #[tokio::test]
    async fn an_execution_outliving_the_unary_timeout_still_delivers_its_completion() {
        let unary_timeout = Duration::from_millis(200);
        let slower_than_unary_timeout = Duration::from_secs(1);
        let execution_id = Uuid::new_v4();
        let url = serve(fake_engine_delayed(execution_id, slower_than_unary_timeout)).await;

        let too_short_to_outlast_the_execution =
            EngineClient::with_timeouts(&url, unary_timeout, unary_timeout)
                .expect("client with a short streaming deadline");
        let mut cut = too_short_to_outlast_the_execution
            .stream_events(&a_principal(), execution_id)
            .await
            .expect("stream");
        assert!(
            cut.recv().await.is_none(),
            "a streaming deadline shorter than the execution cuts the stream before its \
             completion event — this is what left topics stuck at Running"
        );

        let outlasts_the_execution =
            EngineClient::with_timeouts(&url, unary_timeout, Duration::from_secs(30))
                .expect("client");
        let mut events = outlasts_the_execution
            .stream_events(&a_principal(), execution_id)
            .await
            .expect("stream");
        let event = events
            .recv()
            .await
            .expect("the completion event must survive a wait longer than the unary timeout");
        assert_eq!(event.kind, ExecutionEventKind::ExecutionCompleted);
    }

    #[tokio::test]
    async fn every_engine_call_carries_the_principal() {
        let execution_id = Uuid::new_v4();
        let fake = fake_engine(execution_id);
        let url = serve(Arc::clone(&fake)).await;
        let client = EngineClient::new(&url).expect("client");
        let principal = a_principal();

        client
            .start_execution(&principal, "agent", "{}")
            .await
            .expect("start");
        client
            .interrupt(&principal, execution_id, "{}")
            .await
            .expect("interrupt");
        client
            .stream_events(&principal, execution_id)
            .await
            .expect("stream");

        let seen = fake.received_principals.lock().expect("lock");
        assert_eq!(
            seen.len(),
            3,
            "start_execution, interrupt and stream_events"
        );
        for received in seen.iter() {
            assert_eq!(
                received.as_ref(),
                Some(&principal),
                "every Engine call must name the user it is made for"
            );
        }
    }
}
