// Runs a tool node by calling Tool Service.

use buffa::EnumValue;
use common::execution_input::ExecutionInput;
use common::proto::tools::v1::{
    ExecuteRequest, ExecuteStatus, ListToolsRequest, Tool, ToolServiceClient,
};
use connectrpc::client::HttpClient;
use engine_core::{
    LlmOutput, TOOL_RESULT_ERROR_KEY, TOOL_RESULT_STATE_KEY, TaskError, TaskExecutor, ToolCall,
    ToolRecord, llm_tool_call_pointer,
};
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use serde_json::Value;
use std::time::Duration;

const CALL_TIMEOUT: Duration = Duration::from_secs(60);

pub const MAX_CONSECUTIVE_TOOL_ERRORS: usize = 3;
pub const MAX_PARALLEL_TOOL_CALLS: usize = 8;

fn consecutive_tool_errors(state: &Value) -> usize {
    state
        .get(TOOL_RESULT_STATE_KEY)
        .and_then(Value::as_array)
        .map(|results| {
            results
                .iter()
                .rev()
                .take_while(|result| result.get(TOOL_RESULT_ERROR_KEY).is_some())
                .count()
        })
        .unwrap_or(0)
}

fn recoverable_error(state: &Value, message: &str) -> Result<Value, TaskError> {
    if consecutive_tool_errors(state) + 1 >= MAX_CONSECUTIVE_TOOL_ERRORS {
        return Err(TaskError::Failed(format!(
            "{MAX_CONSECUTIVE_TOOL_ERRORS} consecutive tool errors, giving up; last error: {message}"
        )));
    }
    Ok(serde_json::json!({ TOOL_RESULT_ERROR_KEY: message }))
}

pub struct ToolTaskExecutor {
    client: ToolServiceClient<HttpClient>,
}

impl ToolTaskExecutor {
    pub fn new(tool_service_url: &str) -> Result<Self, String> {
        Ok(Self {
            client: crate::executors::grpc_client(
                "TOOL_SERVICE_URL",
                tool_service_url,
                CALL_TIMEOUT,
                ToolServiceClient::new,
            )?,
        })
    }

    pub async fn list_tools(&self) -> Result<Vec<Tool>, String> {
        let response = self
            .client
            .list_tools(ListToolsRequest::default())
            .await
            .map_err(|e| e.to_string())?
            .into_owned();
        Ok(response.tools)
    }
}

impl TaskExecutor for ToolTaskExecutor {
    async fn execute(
        &self,
        kind: &str,
        config: &Value,
        state: &Value,
        idempotency_key: &str,
    ) -> Result<Value, TaskError> {
        debug_assert_eq!(
            crate::dispatch::NodeKind::parse(kind),
            Ok(crate::dispatch::NodeKind::Tool)
        );
        let calls = requested_calls(config, state)?;
        if calls.len() > MAX_PARALLEL_TOOL_CALLS {
            return recoverable_error(
                state,
                &format!(
                    "one turn may call at most {MAX_PARALLEL_TOOL_CALLS} tools at once, got {}",
                    calls.len()
                ),
            );
        }

        let outcomes = self.run_all(&calls, idempotency_key).await?;

        match outcomes.as_slice() {
            [CallOutcome::Output(output)] => Ok(output.clone()),
            [CallOutcome::Error(message)] => recoverable_error(state, message),
            many if many.iter().all(CallOutcome::is_error) => {
                recoverable_error(state, &joined_errors(&calls, many))
            }
            many => Ok(results_by_call(&calls, many)),
        }
    }
}

impl ToolTaskExecutor {
    async fn run_all(
        &self,
        calls: &[ToolCall],
        idempotency_key: &str,
    ) -> Result<Vec<CallOutcome>, TaskError> {
        let mut running: FuturesUnordered<_> = calls
            .iter()
            .enumerate()
            .map(|(index, call)| async move {
                (
                    index,
                    self.call_tool(call, &format!("{idempotency_key}:{index}"))
                        .await,
                )
            })
            .collect();
        let mut finished = Vec::with_capacity(calls.len());
        while let Some((index, outcome)) = running.next().await {
            finished.push((index, outcome?));
        }
        finished.sort_by_key(|(index, _)| *index);
        Ok(finished.into_iter().map(|(_, outcome)| outcome).collect())
    }

    async fn call_tool(
        &self,
        call: &ToolCall,
        idempotency_key: &str,
    ) -> Result<CallOutcome, TaskError> {
        let response = self
            .client
            .execute(ExecuteRequest {
                slug: call.name.clone(),
                input_json: call.args.to_string(),
                idempotency_key: idempotency_key.to_owned(),
                ..Default::default()
            })
            .await
            .map_err(|e| TaskError::Failed(e.to_string()))?
            .into_owned();

        match response.status {
            EnumValue::Known(ExecuteStatus::Ok) => serde_json::from_str(&response.output_json)
                .map(CallOutcome::Output)
                .map_err(|e| TaskError::Failed(e.to_string())),
            EnumValue::Known(ExecuteStatus::RequiresApproval) => Err(TaskError::Failed(
                "tool requires approval — not yet wired in Engine".to_owned(),
            )),
            EnumValue::Known(
                ExecuteStatus::Error | ExecuteStatus::NotExecutable | ExecuteStatus::Unspecified,
            )
            | EnumValue::Unknown(_) => Ok(CallOutcome::Error(response.error_message)),
        }
    }
}

enum CallOutcome {
    Output(Value),
    Error(String),
}

impl CallOutcome {
    fn is_error(&self) -> bool {
        matches!(self, Self::Error(_))
    }
}

fn requested_calls(config: &Value, state: &Value) -> Result<Vec<ToolCall>, TaskError> {
    if let Some(slug) = config.get("tool_slug").and_then(Value::as_str) {
        return Ok(vec![ToolCall {
            name: slug.to_owned(),
            args: serde_json::json!({"query": ExecutionInput::in_state(state).question}),
        }]);
    }
    let requested = LlmOutput::in_state(state).tool_call;
    if requested.is_null() {
        return Err(TaskError::Failed(format!(
            "no tool call at {}",
            llm_tool_call_pointer()
        )));
    }
    let one_or_many = match requested {
        Value::Array(calls) => calls,
        single => vec![single],
    };
    if one_or_many.is_empty() {
        return Err(TaskError::Failed("tool_call is an empty list".to_owned()));
    }
    one_or_many.into_iter().map(parsed_call).collect()
}

fn parsed_call(call: Value) -> Result<ToolCall, TaskError> {
    serde_json::from_value(call)
        .map_err(|error| TaskError::Failed(format!("tool_call is not usable: {error}")))
}

fn results_by_call(calls: &[ToolCall], outcomes: &[CallOutcome]) -> Value {
    let entries: Vec<Value> = calls
        .iter()
        .zip(outcomes)
        .map(|(call, outcome)| {
            let result = match outcome {
                CallOutcome::Output(output) => output.clone(),
                CallOutcome::Error(message) => {
                    serde_json::json!({ TOOL_RESULT_ERROR_KEY: message })
                }
            };
            ToolRecord::of(call, result, chrono::Utc::now().to_rfc3339()).to_json()
        })
        .collect();
    serde_json::json!({"calls": entries})
}

fn joined_errors(calls: &[ToolCall], outcomes: &[CallOutcome]) -> String {
    let reasons: Vec<String> = calls
        .iter()
        .zip(outcomes)
        .filter_map(|(call, outcome)| match outcome {
            CallOutcome::Error(message) => Some(format!("{}: {message}", call.name)),
            CallOutcome::Output(_) => None,
        })
        .collect();
    format!("every tool call failed — {}", reasons.join("; "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::proto::tools::v1::{
        ActivateToolRequest, ActivateToolResponse, CreateToolRequest, CreateToolResponse,
        ExecuteResponse, ListToolsResponse, ToolService, ValidateToolRequest, ValidateToolResponse,
    };
    use connectrpc::{
        RequestContext, Response, Router as ConnectRouter, ServiceRequest, ServiceResult,
    };
    use std::sync::{Arc, Mutex};

    struct FakeToolService {
        received: Mutex<Vec<ExecuteRequest>>,
        status: ExecuteStatus,
        output_json: String,
        error_message: String,
    }

    impl FakeToolService {
        fn ok(output_json: &str) -> Self {
            Self {
                received: Mutex::new(Vec::new()),
                status: ExecuteStatus::Ok,
                output_json: output_json.to_owned(),
                error_message: String::new(),
            }
        }

        fn failing(status: ExecuteStatus, error_message: &str) -> Self {
            Self {
                received: Mutex::new(Vec::new()),
                status,
                output_json: String::new(),
                error_message: error_message.to_owned(),
            }
        }
    }

    #[allow(refining_impl_trait)]
    impl ToolService for FakeToolService {
        async fn execute(
            &self,
            _ctx: RequestContext,
            request: ServiceRequest<'_, ExecuteRequest>,
        ) -> ServiceResult<ExecuteResponse> {
            self.received
                .lock()
                .expect("lock")
                .push(request.to_owned_message());
            Response::ok(ExecuteResponse {
                status: EnumValue::Known(self.status),
                output_json: self.output_json.clone(),
                error_message: self.error_message.clone(),
                ..Default::default()
            })
        }
        async fn list_tools(
            &self,
            _ctx: RequestContext,
            _request: ServiceRequest<'_, ListToolsRequest>,
        ) -> ServiceResult<ListToolsResponse> {
            Response::ok(ListToolsResponse {
                tools: vec![Tool {
                    slug: "web_search".to_owned(),
                    ..Default::default()
                }],
                ..Default::default()
            })
        }
        async fn create_tool(
            &self,
            _ctx: RequestContext,
            _request: ServiceRequest<'_, CreateToolRequest>,
        ) -> ServiceResult<CreateToolResponse> {
            Response::ok(CreateToolResponse::default())
        }
        async fn validate_tool(
            &self,
            _ctx: RequestContext,
            _request: ServiceRequest<'_, ValidateToolRequest>,
        ) -> ServiceResult<ValidateToolResponse> {
            Response::ok(ValidateToolResponse::default())
        }
        async fn activate_tool(
            &self,
            _ctx: RequestContext,
            _request: ServiceRequest<'_, ActivateToolRequest>,
        ) -> ServiceResult<ActivateToolResponse> {
            Response::ok(ActivateToolResponse::default())
        }
    }

    async fn serve(fake: Arc<FakeToolService>) -> String {
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
    async fn execute_reads_a_config_fixed_tool_slug_and_searches_the_question() {
        let fake = Arc::new(FakeToolService::ok(r#"{"results": []}"#));
        let url = serve(Arc::clone(&fake)).await;
        let executor = ToolTaskExecutor::new(&url).expect("client");
        let config = serde_json::json!({"tool_slug": "kb_search"});
        let state = serde_json::json!({"question": "what is claude code?"});

        let output = executor
            .execute("tool", &config, &state, "e:tool:0")
            .await
            .expect("execute");

        assert_eq!(output, serde_json::json!({"results": []}));
        let received = fake.received.lock().expect("lock");
        assert_eq!(received[0].slug, "kb_search");
        assert_eq!(
            received[0].input_json,
            r#"{"query":"what is claude code?"}"#
        );
    }

    #[tokio::test]
    async fn execute_reads_the_tool_call_from_state_and_returns_the_output() {
        let fake = Arc::new(FakeToolService::ok(r#"{"results": []}"#));
        let url = serve(Arc::clone(&fake)).await;
        let executor = ToolTaskExecutor::new(&url).expect("client");
        let state = serde_json::json!({"llm": {"tool_call": {"name": "web_search", "args": {"query": "rust"}}}});

        let output = executor
            .execute("tool", &serde_json::json!({}), &state, "e:tool:0")
            .await
            .expect("execute");

        assert_eq!(output, serde_json::json!({"results": []}));
        let received = fake.received.lock().expect("lock");
        assert_eq!(received[0].slug, "web_search");
        assert_eq!(received[0].input_json, r#"{"query":"rust"}"#);
    }

    #[tokio::test]
    async fn a_list_of_tool_calls_runs_every_one_and_labels_each_result() {
        let fake = Arc::new(FakeToolService::ok(r#"{"title": "page"}"#));
        let url = serve(Arc::clone(&fake)).await;
        let executor = ToolTaskExecutor::new(&url).expect("client");
        let state = serde_json::json!({"llm": {"tool_call": [
            {"name": "web_fetch", "args": {"url": "https://first.example/a"}},
            {"name": "web_fetch", "args": {"url": "https://second.example/b"}},
        ]}});

        let output = executor
            .execute("tool", &serde_json::json!({}), &state, "e:tool:0")
            .await
            .expect("a list of calls is not a failure");

        let calls = output["calls"].as_array().expect("one entry per call");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["name"], serde_json::json!("web_fetch"));
        assert_eq!(
            calls[0]["args"]["url"],
            serde_json::json!("https://first.example/a")
        );
        assert_eq!(calls[0]["result"], serde_json::json!({"title": "page"}));
        assert_eq!(
            calls[1]["args"]["url"],
            serde_json::json!("https://second.example/b")
        );
        assert_eq!(fake.received.lock().expect("lock").len(), 2);
    }

    #[tokio::test]
    async fn a_list_holding_one_call_keeps_the_plain_single_result_shape() {
        let fake = Arc::new(FakeToolService::ok(r#"{"results": []}"#));
        let url = serve(Arc::clone(&fake)).await;
        let executor = ToolTaskExecutor::new(&url).expect("client");
        let state = serde_json::json!({"llm": {"tool_call": [{"name": "web_search", "args": {"query": "rust"}}]}});

        let output = executor
            .execute("tool", &serde_json::json!({}), &state, "e:tool:0")
            .await
            .expect("execute");

        assert_eq!(output, serde_json::json!({"results": []}));
    }

    #[tokio::test]
    async fn a_list_whose_calls_all_failed_is_one_recoverable_error_naming_each() {
        let fake = Arc::new(FakeToolService::failing(
            ExecuteStatus::Error,
            "invalid_argument: url is required",
        ));
        let url = serve(fake).await;
        let executor = ToolTaskExecutor::new(&url).expect("client");
        let state = serde_json::json!({"llm": {"tool_call": [
            {"name": "web_fetch", "args": {}},
            {"name": "web_search", "args": {}},
        ]}});

        let output = executor
            .execute("tool", &serde_json::json!({}), &state, "e:tool:0")
            .await
            .expect("a failed batch must not fail the node");

        let error = output["error"].as_str().expect("one joined error");
        assert!(error.contains("web_fetch"), "{error}");
        assert!(error.contains("web_search"), "{error}");
        assert!(error.contains("url is required"), "{error}");
    }

    #[tokio::test]
    async fn a_list_longer_than_the_parallel_limit_is_rejected_by_naming_it() {
        let fake = Arc::new(FakeToolService::ok("{}"));
        let url = serve(Arc::clone(&fake)).await;
        let executor = ToolTaskExecutor::new(&url).expect("client");
        let calls: Vec<Value> = (0..=MAX_PARALLEL_TOOL_CALLS)
            .map(|index| serde_json::json!({"name": "web_fetch", "args": {"url": index}}))
            .collect();
        let state = serde_json::json!({"llm": {"tool_call": calls}});

        let output = executor
            .execute("tool", &serde_json::json!({}), &state, "e:tool:0")
            .await
            .expect("over the limit is an observation, not a crash");

        let error = output["error"].as_str().expect("an error naming the limit");
        assert!(
            error.contains(&MAX_PARALLEL_TOOL_CALLS.to_string()),
            "{error}"
        );
        assert!(fake.received.lock().expect("lock").is_empty());
    }

    #[tokio::test]
    async fn execute_maps_requires_approval_to_a_clear_error() {
        let fake = Arc::new(FakeToolService::failing(
            ExecuteStatus::RequiresApproval,
            "",
        ));
        let url = serve(fake).await;
        let executor = ToolTaskExecutor::new(&url).expect("client");
        let state = serde_json::json!({"llm": {"tool_call": {"name": "send_email", "args": {}}}});

        let error = executor
            .execute("tool", &serde_json::json!({}), &state, "e:tool:0")
            .await
            .unwrap_err();

        assert!(matches!(error, TaskError::Failed(msg) if msg.contains("requires approval")));
    }

    #[tokio::test]
    async fn a_tool_error_status_becomes_a_recoverable_error_observation() {
        let fake = Arc::new(FakeToolService::failing(
            ExecuteStatus::Error,
            "invalid_argument: query is required",
        ));
        let url = serve(fake).await;
        let executor = ToolTaskExecutor::new(&url).expect("client");
        let state = serde_json::json!({"llm": {"tool_call": {"name": "kb_search", "args": {}}}});

        let output = executor
            .execute("tool", &serde_json::json!({}), &state, "e:tool:0")
            .await
            .expect("a tool error must not fail the node");

        assert_eq!(
            output,
            serde_json::json!({"error": "invalid_argument: query is required"})
        );
    }

    #[tokio::test]
    async fn the_third_consecutive_tool_error_fails_the_node_with_the_last_error() {
        let fake = Arc::new(FakeToolService::failing(
            ExecuteStatus::Error,
            "error sending request for url (https://www.accuweather.com/)",
        ));
        let url = serve(fake).await;
        let executor = ToolTaskExecutor::new(&url).expect("client");
        let state = serde_json::json!({
            "llm": {"tool_call": {"name": "web_fetch", "args": {"url": "https://www.accuweather.com/"}}},
            "tool_result": [{"error": "first"}, {"error": "second"}],
        });

        let error = executor
            .execute("tool", &serde_json::json!({}), &state, "e:tool:0")
            .await
            .unwrap_err();

        let TaskError::Failed(message) = error;
        assert!(message.contains("3 consecutive tool errors"), "{message}");
        assert!(message.contains("accuweather.com"), "{message}");
    }

    #[tokio::test]
    async fn a_successful_call_in_between_resets_the_consecutive_error_run() {
        let fake = Arc::new(FakeToolService::failing(
            ExecuteStatus::Error,
            "still broken",
        ));
        let url = serve(fake).await;
        let executor = ToolTaskExecutor::new(&url).expect("client");
        let state = serde_json::json!({
            "llm": {"tool_call": {"name": "web_fetch", "args": {}}},
            "tool_result": [{"error": "first"}, {"results": []}, {"error": "second"}],
        });

        let output = executor
            .execute("tool", &serde_json::json!({}), &state, "e:tool:0")
            .await
            .expect("only one error in the tail, so still recoverable");

        assert_eq!(output, serde_json::json!({"error": "still broken"}));
    }

    #[tokio::test]
    async fn an_unreachable_tool_service_stays_a_hard_failure() {
        let executor = ToolTaskExecutor::new("http://127.0.0.1:1").expect("client");
        let state = serde_json::json!({"llm": {"tool_call": {"name": "web_search", "args": {}}}});

        let error = executor
            .execute("tool", &serde_json::json!({}), &state, "e:tool:0")
            .await
            .unwrap_err();

        let TaskError::Failed(message) = error;
        assert!(!message.contains("consecutive tool errors"), "{message}");
    }

    #[tokio::test]
    async fn list_tools_returns_the_catalog() {
        let fake = Arc::new(FakeToolService::ok("{}"));
        let url = serve(fake).await;
        let executor = ToolTaskExecutor::new(&url).expect("client");

        let tools = executor.list_tools().await.expect("list");

        assert_eq!(tools[0].slug, "web_search");
    }
}
