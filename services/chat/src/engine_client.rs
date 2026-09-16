// Thin wrapper over the engine.v1.EngineService this stack already runs — same
// client-construction pattern as services/tool/src/tools/kb_client.rs uses for knowledge-base.

use common::proto::engine::v1::{
    EngineServiceClient, ExecutionEvent, InterruptRequest, StartExecutionRequest,
    StreamEventsRequest,
};
use connectrpc::Protocol;
use connectrpc::client::{CallOptions, ClientConfig, HttpClient};
use std::time::Duration;
use tokio::sync::mpsc;
use uuid::Uuid;

const CALL_TIMEOUT: Duration = Duration::from_secs(20);
/// `StreamEvents` needs its own, far longer deadline. A client timeout is a *whole-call*
/// deadline, and connectrpc enforces it on every frame poll of a streaming call, not just on
/// connect (`connectrpc::client`'s `poll_body` wraps each frame in `with_deadline`). Under
/// `CALL_TIMEOUT` any Engine execution running longer than 20s had its event stream cut
/// mid-flight, so `TopicManager::finish_topic` never ran and the topic stuck at `Running`
/// forever. `CallOptions` has no "no deadline" setting (an unset per-call timeout just falls
/// back to the client's default), so this is a long finite bound instead.
const STREAM_TIMEOUT: Duration = Duration::from_secs(3_600);

pub struct EngineClient {
    inner: EngineServiceClient<HttpClient>,
    stream_timeout: Duration,
}

impl EngineClient {
    pub fn new(engine_url: &str) -> Result<Self, String> {
        Self::with_timeouts(engine_url, CALL_TIMEOUT, STREAM_TIMEOUT)
    }

    /// `new`, with both deadlines given explicitly — tests use it to drive the gap between the
    /// short unary timeout and the long streaming one without waiting out the real values.
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

    pub async fn start_execution(&self, graph_id: &str, input_json: &str) -> Result<Uuid, String> {
        let response = self
            .inner
            .start_execution(StartExecutionRequest {
                graph_id: graph_id.to_owned(),
                input_json: input_json.to_owned(),
                ..Default::default()
            })
            .await
            .map_err(|e| e.to_string())?
            .into_owned();
        Uuid::parse_str(&response.execution_id).map_err(|e| e.to_string())
    }

    pub async fn interrupt(&self, execution_id: Uuid, input_json: &str) -> Result<(), String> {
        self.inner
            .interrupt(InterruptRequest {
                execution_id: execution_id.to_string(),
                input_json: input_json.to_owned(),
                ..Default::default()
            })
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Opens the stream, then drains it in a spawned task forwarding each event through the
    /// returned channel — the receiver ends (`recv()` returns `None`) when the stream ends
    /// cleanly, errors, or every receiver is dropped, whichever comes first.
    pub async fn stream_events(
        &self,
        execution_id: Uuid,
    ) -> Result<mpsc::UnboundedReceiver<ExecutionEvent>, String> {
        let mut stream = self
            .inner
            .stream_events_with_options(
                StreamEventsRequest {
                    execution_id: Some(execution_id.to_string()),
                    user_id: None,
                    ..Default::default()
                },
                CallOptions::default().with_timeout(self.stream_timeout),
            )
            .await
            .map_err(|e| e.to_string())?;
        let (sender, receiver) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            loop {
                match stream.message::<ExecutionEvent>().await {
                    Ok(Some(item)) => {
                        if sender.send(item.to_owned_message()).is_err() {
                            break; // receiver dropped — nobody's listening anymore
                        }
                    }
                    Ok(None) => break, // clean end of stream
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
        /// How long `stream_events` waits before emitting its one event — stands in for an
        /// Engine execution that takes a while to finish.
        stream_delay: Duration,
    }

    fn fake_engine(execution_id: Uuid) -> Arc<FakeEngine> {
        fake_engine_delayed(execution_id, Duration::ZERO)
    }

    fn fake_engine_delayed(execution_id: Uuid, stream_delay: Duration) -> Arc<FakeEngine> {
        Arc::new(FakeEngine {
            execution_id: execution_id.to_string(),
            received_interrupts: Mutex::new(Vec::new()),
            stream_delay,
        })
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
            _ctx: RequestContext,
            _request: ServiceRequest<'_, StartExecutionRequest>,
        ) -> ServiceResult<StartExecutionResponse> {
            Response::ok(StartExecutionResponse {
                execution_id: self.execution_id.clone(),
                ..Default::default()
            })
        }
        async fn interrupt(
            &self,
            _ctx: RequestContext,
            request: ServiceRequest<'_, InterruptRequest>,
        ) -> ServiceResult<InterruptResponse> {
            self.received_interrupts
                .lock()
                .expect("lock")
                .push(request.to_owned_message());
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
            _ctx: RequestContext,
            _request: ServiceRequest<'_, StreamEventsRequest>,
        ) -> ServiceResult<ServiceStream<ExecutionEvent>> {
            let event = ExecutionEvent {
                id: "e1".to_owned(),
                execution_id: self.execution_id.clone(),
                payload_kind: "ExecutionCompleted".to_owned(),
                payload_json: r#"{"final_state":{}}"#.to_owned(),
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
            .start_execution("agent", r#"{"question": "hi"}"#)
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
            .interrupt(execution_id, r#"{"question": "more"}"#)
            .await
            .expect("interrupt");

        let received = fake.received_interrupts.lock().expect("lock");
        assert_eq!(received[0].execution_id, execution_id.to_string());
        assert_eq!(received[0].input_json, r#"{"question": "more"}"#);
    }

    #[tokio::test]
    async fn stream_events_yields_the_engine_stream() {
        let execution_id = Uuid::new_v4();
        let fake = fake_engine(execution_id);
        let url = serve(fake).await;
        let client = EngineClient::new(&url).expect("client");

        let mut events = client.stream_events(execution_id).await.expect("stream");
        let event = events.recv().await.expect("one event");

        assert_eq!(event.payload_kind, "ExecutionCompleted");
        assert!(
            events.recv().await.is_none(),
            "stream ends cleanly after one event"
        );
    }

    /// The finding: a client timeout is a whole-call deadline that connectrpc re-applies to
    /// every frame of a *streaming* call, so an execution outliving the unary timeout had its
    /// event stream cut before `ExecutionCompleted` ever arrived — leaving the topic `Running`
    /// forever. The two halves below are the same fake and the same wait; only the streaming
    /// deadline differs.
    #[tokio::test]
    async fn an_execution_outliving_the_unary_timeout_still_delivers_its_completion() {
        let unary_timeout = Duration::from_millis(200);
        let slower_than_unary_timeout = Duration::from_secs(1);
        let execution_id = Uuid::new_v4();
        let url = serve(fake_engine_delayed(execution_id, slower_than_unary_timeout)).await;

        // The regression, reproduced: the short deadline applied to the streaming call too.
        let short = EngineClient::with_timeouts(&url, unary_timeout, unary_timeout)
            .expect("client with a short streaming deadline");
        let mut cut = short.stream_events(execution_id).await.expect("stream");
        assert!(
            cut.recv().await.is_none(),
            "a streaming deadline shorter than the execution cuts the stream before its \
             completion event — this is what left topics stuck at Running"
        );

        // The fix: the streaming call carries its own, much longer deadline.
        let client = EngineClient::with_timeouts(&url, unary_timeout, Duration::from_secs(30))
            .expect("client");
        let mut events = client.stream_events(execution_id).await.expect("stream");
        let event = events
            .recv()
            .await
            .expect("the completion event must survive a wait longer than the unary timeout");
        assert_eq!(event.payload_kind, "ExecutionCompleted");
    }
}
