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
use serde_json::Value;
use std::time::Duration;

const CALL_TIMEOUT: Duration = Duration::from_secs(60);

pub const MAX_CONSECUTIVE_TOOL_ERRORS: usize = 3;
pub const MAX_PARALLEL_TOOL_CALLS: usize = 8;

const THE_REQUEST_AS_A_WHOLE: &str = "tool_call";

fn consecutive_failed_steps(state: &Value) -> usize {
    ToolRecord::all_under(state, TOOL_RESULT_STATE_KEY)
        .chunk_by(ToolRecord::made_in_the_same_step_as)
        .rev()
        .take_while(|step| nothing_reached_its_source(step))
        .count()
}

fn nothing_reached_its_source(step: &[ToolRecord]) -> bool {
    step.iter().all(|record| !record.reached_its_source())
}

fn failed(call: &ToolCall, message: &str, fetched_at: String) -> ToolRecord {
    ToolRecord::of(
        call,
        serde_json::json!({ TOOL_RESULT_ERROR_KEY: message }),
        fetched_at,
    )
}

fn recoverable_unless_the_run_is_too_long(
    state: &Value,
    this_step: Vec<ToolRecord>,
) -> Result<Value, TaskError> {
    if nothing_reached_its_source(&this_step)
        && consecutive_failed_steps(state) + 1 >= MAX_CONSECUTIVE_TOOL_ERRORS
    {
        let last_error = this_step
            .last()
            .and_then(|record| record.result.get(TOOL_RESULT_ERROR_KEY))
            .and_then(Value::as_str)
            .unwrap_or_default();
        return Err(TaskError::Failed(format!(
            "{MAX_CONSECUTIVE_TOOL_ERRORS} consecutive tool errors, giving up; last error: {last_error}"
        )));
    }
    Ok(Value::Array(
        this_step.iter().map(ToolRecord::to_json).collect(),
    ))
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
            return recoverable_unless_the_run_is_too_long(state, vec![refused_whole(&calls)]);
        }

        let outcomes = self.run_all(&calls, idempotency_key).await?;
        recoverable_unless_the_run_is_too_long(state, records_of(&calls, outcomes))
    }
}

impl ToolTaskExecutor {
    async fn run_all(
        &self,
        calls: &[ToolCall],
        idempotency_key: &str,
    ) -> Result<Vec<CallOutcome>, TaskError> {
        futures::future::try_join_all(calls.iter().enumerate().map(|(index, call)| async move {
            self.call_tool(call, &format!("{idempotency_key}:{index}"))
                .await
        }))
        .await
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

fn refused_whole(calls: &[ToolCall]) -> ToolRecord {
    let the_request = ToolCall {
        name: THE_REQUEST_AS_A_WHOLE.to_owned(),
        args: serde_json::to_value(calls).unwrap_or(Value::Null),
    };
    failed(
        &the_request,
        &format!(
            "one turn may call at most {MAX_PARALLEL_TOOL_CALLS} tools at once, got {}",
            calls.len()
        ),
        chrono::Utc::now().to_rfc3339(),
    )
}

fn records_of(calls: &[ToolCall], outcomes: Vec<CallOutcome>) -> Vec<ToolRecord> {
    let fetched_at = chrono::Utc::now().to_rfc3339();
    calls
        .iter()
        .zip(outcomes)
        .map(|(call, outcome)| match outcome {
            CallOutcome::Output(output) => ToolRecord::of(call, output, fetched_at.clone()),
            CallOutcome::Error(message) => failed(call, &message, fetched_at.clone()),
        })
        .collect()
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

    fn one_record(output: &Value) -> ToolRecord {
        let records = output
            .as_array()
            .expect("the tool node returns its records as a list");
        assert_eq!(records.len(), 1, "one call is one record: {output}");
        ToolRecord::read(&records[0]).expect("every entry is a whole record")
    }

    fn recorded_in_step(step: u32, result: Value) -> Value {
        ToolRecord::of(
            &ToolCall {
                name: "web_fetch".to_owned(),
                args: serde_json::json!({}),
            },
            result,
            format!("2026-09-22T10:0{step}:00+00:00"),
        )
        .to_json()
    }

    fn failed_in_step(step: u32, message: &str) -> Value {
        recorded_in_step(step, serde_json::json!({ TOOL_RESULT_ERROR_KEY: message }))
    }

    fn reached_in_step(step: u32) -> Value {
        recorded_in_step(step, serde_json::json!({"results": []}))
    }

    fn three_parallel_calls() -> Value {
        serde_json::json!([
            {"name": "weather_now", "args": {"place": "Bishkek"}},
            {"name": "weather_now", "args": {"place": "Osh"}},
            {"name": "weather_now", "args": {"place": "Naryn"}},
        ])
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

        let record = one_record(&output);
        assert_eq!(record.name, "kb_search");
        assert_eq!(
            record.args,
            serde_json::json!({"query": "what is claude code?"})
        );
        assert_eq!(record.result, serde_json::json!({"results": []}));
        let received = fake.received.lock().expect("lock");
        assert_eq!(received[0].slug, "kb_search");
        assert_eq!(
            received[0].input_json,
            r#"{"query":"what is claude code?"}"#
        );
    }

    #[tokio::test]
    async fn one_call_becomes_one_record_carrying_the_call_and_when_it_was_fetched() {
        let fake = Arc::new(FakeToolService::ok(r#"{"results": []}"#));
        let url = serve(Arc::clone(&fake)).await;
        let executor = ToolTaskExecutor::new(&url).expect("client");
        let state = serde_json::json!({"llm": {"tool_call": {"name": "web_search", "args": {"query": "rust"}}}});

        let output = executor
            .execute("tool", &serde_json::json!({}), &state, "e:tool:0")
            .await
            .expect("execute");

        let record = one_record(&output);
        assert_eq!(record.name, "web_search");
        assert_eq!(record.args, serde_json::json!({"query": "rust"}));
        assert_eq!(record.result, serde_json::json!({"results": []}));
        assert!(
            chrono::DateTime::parse_from_rfc3339(&record.fetched_at).is_ok(),
            "a follow-up decides whether this is still fresh from when it was fetched"
        );
        let received = fake.received.lock().expect("lock");
        assert_eq!(received[0].slug, "web_search");
        assert_eq!(received[0].input_json, r#"{"query":"rust"}"#);
    }

    #[tokio::test]
    async fn a_list_of_tool_calls_becomes_one_record_per_call() {
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

        let records: Vec<ToolRecord> = output
            .as_array()
            .expect("a list of records")
            .iter()
            .map(|record| ToolRecord::read(record).expect("every entry is a whole record"))
            .collect();
        assert_eq!(
            records.len(),
            2,
            "two lookups are two records, so the window over the last few never fuses them"
        );
        assert_eq!(
            records[0].args["url"],
            serde_json::json!("https://first.example/a")
        );
        assert_eq!(records[0].result, serde_json::json!({"title": "page"}));
        assert_eq!(
            records[1].args["url"],
            serde_json::json!("https://second.example/b")
        );
        assert_eq!(fake.received.lock().expect("lock").len(), 2);
    }

    #[tokio::test]
    async fn a_list_whose_calls_all_failed_records_what_each_one_attempted() {
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

        let records = output.as_array().expect("a list of records");
        assert_eq!(records[0]["name"], serde_json::json!("web_fetch"));
        assert_eq!(records[1]["name"], serde_json::json!("web_search"));
        for record in records {
            assert_eq!(
                record["result"]["error"],
                serde_json::json!("invalid_argument: url is required"),
                "each failed lookup says why it failed beside what it asked for"
            );
        }
    }

    #[tokio::test]
    async fn a_list_longer_than_the_parallel_limit_is_refused_as_one_request() {
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

        let record = one_record(&output);
        let error = record.result["error"]
            .as_str()
            .expect("an error naming the limit");
        assert!(
            error.contains(&MAX_PARALLEL_TOOL_CALLS.to_string()),
            "{error}"
        );
        assert_eq!(
            record.args.as_array().map(Vec::len),
            Some(MAX_PARALLEL_TOOL_CALLS + 1),
            "the refused request is recorded whole, so the model sees what it asked for"
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
    async fn a_tool_error_status_becomes_a_recoverable_error_record_naming_the_call() {
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

        let record = one_record(&output);
        assert_eq!(record.name, "kb_search");
        assert_eq!(
            record.result,
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
            "tool_result": [failed_in_step(1, "first"), failed_in_step(2, "second")],
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
            "tool_result": [failed_in_step(1, "first"), reached_in_step(2), failed_in_step(3, "second")],
        });

        let output = executor
            .execute("tool", &serde_json::json!({}), &state, "e:tool:0")
            .await
            .expect("only one error in the tail, so still recoverable");

        assert_eq!(
            one_record(&output).result,
            serde_json::json!({"error": "still broken"})
        );
    }

    #[tokio::test]
    async fn one_step_whose_parallel_calls_all_failed_is_one_failure_not_three() {
        let fake = Arc::new(FakeToolService::failing(
            ExecuteStatus::Error,
            "upstream down",
        ));
        let url = serve(fake).await;
        let executor = ToolTaskExecutor::new(&url).expect("client");
        let state = serde_json::json!({"llm": {"tool_call": three_parallel_calls()}});

        let output = executor
            .execute("tool", &serde_json::json!({}), &state, "e:tool:0")
            .await
            .expect("the first failed step leaves the model room to try another way");

        assert_eq!(output.as_array().map(Vec::len), Some(3));
    }

    #[tokio::test]
    async fn the_third_failed_step_gives_up_however_many_lookups_each_step_made() {
        let fake = Arc::new(FakeToolService::failing(
            ExecuteStatus::Error,
            "upstream down",
        ));
        let url = serve(fake).await;
        let executor = ToolTaskExecutor::new(&url).expect("client");
        let state = serde_json::json!({
            "llm": {"tool_call": three_parallel_calls()},
            "tool_result": [
                failed_in_step(1, "first"), failed_in_step(1, "first"),
                failed_in_step(2, "second"), failed_in_step(2, "second"),
            ],
        });

        let TaskError::Failed(message) = executor
            .execute("tool", &serde_json::json!({}), &state, "e:tool:0")
            .await
            .unwrap_err();

        assert!(message.contains("3 consecutive tool errors"), "{message}");
    }

    #[tokio::test]
    async fn a_step_that_got_any_data_breaks_the_run_of_failures() {
        let fake = Arc::new(FakeToolService::failing(
            ExecuteStatus::Error,
            "upstream down",
        ));
        let url = serve(fake).await;
        let executor = ToolTaskExecutor::new(&url).expect("client");
        let state = serde_json::json!({
            "llm": {"tool_call": {"name": "web_fetch", "args": {}}},
            "tool_result": [reached_in_step(1), failed_in_step(1, "a"), failed_in_step(1, "b")],
        });

        let output = executor
            .execute("tool", &serde_json::json!({}), &state, "e:tool:0")
            .await
            .expect("the step before got data, so this is the first failed step, not the third");

        assert_eq!(
            one_record(&output).result,
            serde_json::json!({"error": "upstream down"})
        );
    }

    #[tokio::test]
    async fn every_lookup_of_one_step_carries_the_same_moment_it_was_fetched() {
        let fake = Arc::new(FakeToolService::failing(
            ExecuteStatus::Error,
            "upstream down",
        ));
        let url = serve(fake).await;
        let executor = ToolTaskExecutor::new(&url).expect("client");
        let state = serde_json::json!({"llm": {"tool_call": three_parallel_calls()}});

        let output = executor
            .execute("tool", &serde_json::json!({}), &state, "e:tool:0")
            .await
            .expect("recoverable");

        let records = ToolRecord::all_under(&serde_json::json!({"r": output}), "r");
        assert!(
            records
                .windows(2)
                .all(|pair| pair[0].made_in_the_same_step_as(&pair[1])),
            "the step a record came from is read from its moment, failed lookups included"
        );
    }

    fn a_turn_waiting_on_its_tool_node(tool_call: Value) -> engine_core::Execution {
        engine_core::Execution {
            id: engine_core::ExecutionId(uuid::Uuid::new_v4()),
            graph_id: engine_core::GraphId("agent".to_owned()),
            graph_version: 1,
            user_id: None,
            status: engine_core::Status::Running,
            current_nodes: vec![engine_core::ActiveNode::plain(engine_core::NodeId(
                "tool".to_owned(),
            ))],
            state: serde_json::json!({
                "question": "what is the weather in Bishkek?",
                "llm": {"tool_call": tool_call},
            }),
            iteration: 1,
            max_iterations: 10,
            deadline: None,
            budget: engine_core::Budget::new(10_000, 100, Duration::from_secs(3600)),
        }
    }

    async fn what_a_follow_up_is_handed(output_json: &str, tool_call: Value) -> Value {
        let fake = Arc::new(FakeToolService::ok(output_json));
        let url = serve(fake).await;
        let executor = ToolTaskExecutor::new(&url).expect("client");
        let turn = a_turn_waiting_on_its_tool_node(tool_call);
        let output = executor
            .execute("tool", &serde_json::json!({}), &turn.state, "e:tool:0")
            .await
            .expect("the lookup succeeds");
        let (earlier_turn, _) = engine_core::step(
            &engine_core::agent_graph(),
            turn,
            vec![engine_core::NodeOutput {
                node: engine_core::ActiveNode::plain(engine_core::NodeId("tool".to_owned())),
                result: Ok(output),
            }],
            chrono::Utc::now(),
        );
        serde_json::json!({
            "question": "and the humidity?",
            engine_core::PRIOR_MATERIAL_STATE_KEY: crate::service::carried_records(&earlier_turn.state),
        })
    }

    #[tokio::test]
    async fn what_the_tool_node_fetched_reaches_a_follow_up_fresh_and_showing_what_was_asked() {
        let follow_up = what_a_follow_up_is_handed(
            r#"{"temp": 13.11, "humidity": 64}"#,
            serde_json::json!({"name": "weather_now", "args": {"place": "Bishkek"}}),
        )
        .await;

        let fresh = crate::executors::llm::Material::of(&follow_up)
            .only_what_was_carried_and_is_still_fresh()
            .carried;
        assert_eq!(
            fresh.len(),
            1,
            "a lookup the earlier turn made moments ago must survive the freshness filter, or the \
             follow-up declines with the answer sitting in its state: {follow_up}"
        );
        assert_eq!(fresh[0].result["humidity"], serde_json::json!(64));
        let shown = crate::executors::llm::Material::of(&follow_up)
            .rendered_carried()
            .expect("the carried lookup is shown to the model");
        assert!(
            shown.contains(r#"(asked for {"place":"Bishkek"})"#),
            "the model must see what the carried result was asked for, so it can say which place \
             a reading belongs to: {shown}"
        );
        assert!(shown.contains("13.11"), "{shown}");
    }

    #[tokio::test]
    async fn several_lookups_in_one_step_reach_a_follow_up_as_several_fresh_records() {
        let follow_up = what_a_follow_up_is_handed(
            r#"{"temp": 13.11}"#,
            serde_json::json!([
                {"name": "weather_now", "args": {"place": "Bishkek"}},
                {"name": "weather_now", "args": {"place": "Osh"}},
            ]),
        )
        .await;

        let fresh = crate::executors::llm::Material::of(&follow_up)
            .only_what_was_carried_and_is_still_fresh()
            .carried;
        let places: Vec<&Value> = fresh.iter().map(|record| &record.args["place"]).collect();
        assert_eq!(
            places,
            vec![&serde_json::json!("Bishkek"), &serde_json::json!("Osh")],
            "each lookup is carried as its own fresh record, never fused into one entry"
        );
    }

    #[tokio::test]
    async fn a_batch_larger_than_the_window_reaches_a_follow_up_whole() {
        let cities: Vec<Value> = (0..MAX_PARALLEL_TOOL_CALLS)
            .map(|city| serde_json::json!({"name": "weather_now", "args": {"place": city}}))
            .collect();

        let follow_up =
            what_a_follow_up_is_handed(r#"{"temp": 13.11}"#, Value::Array(cities)).await;

        let carried = crate::executors::llm::Material::of(&follow_up).carried;
        assert_eq!(
            carried.len(),
            MAX_PARALLEL_TOOL_CALLS,
            "a follow-up about any of the cities looked up together can answer from it"
        );
    }

    #[tokio::test]
    async fn a_lookup_that_failed_is_recorded_in_the_turn_but_never_carried_as_material() {
        let fake = Arc::new(FakeToolService::failing(
            ExecuteStatus::Error,
            "upstream down",
        ));
        let url = serve(fake).await;
        let executor = ToolTaskExecutor::new(&url).expect("client");
        let turn = a_turn_waiting_on_its_tool_node(
            serde_json::json!({"name": "weather_now", "args": {"place": "Bishkek"}}),
        );
        let output = executor
            .execute("tool", &serde_json::json!({}), &turn.state, "e:tool:0")
            .await
            .expect("a tool error is an observation");
        let (earlier_turn, _) = engine_core::step(
            &engine_core::agent_graph(),
            turn,
            vec![engine_core::NodeOutput {
                node: engine_core::ActiveNode::plain(engine_core::NodeId("tool".to_owned())),
                result: Ok(output),
            }],
            chrono::Utc::now(),
        );

        let recorded = ToolRecord::all_under(&earlier_turn.state, TOOL_RESULT_STATE_KEY);
        assert_eq!(
            recorded[0].args,
            serde_json::json!({"place": "Bishkek"}),
            "the turn itself still sees what was attempted beside why it failed"
        );
        assert!(
            crate::service::carried_records(&earlier_turn.state).is_empty(),
            "an error is not material a follow-up can answer from"
        );
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
