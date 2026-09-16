// TaskExecutor for kind="tool": calls Tool Service's Execute. Two graphs give it the call two
// different ways: agent_graph's tool node has empty config and an LLM decides the call, so it
// lands at state["llm"]["tool_call"] (see engine-core/src/builder.rs's doc comment on that
// node); rag_graph's tool node IS the entry point — no llm node has run yet, so there is no
// state["llm"] to read — and instead fixes its slug in config["tool_slug"], searching
// state["question"] (StartExecution's own initial state). A config-given tool_slug wins when
// present; otherwise this falls back to the state["llm"]["tool_call"] convention.
//
// Errors split two ways. A tool that *ran* and said no (invalid_argument, a site that blocks
// bots, a timeout) is an observation, returned as Ok({"error": msg}) so the graph's tool → llm
// edge carries it back to the model, which can retry with fixed arguments or pick another tool;
// MAX_CONSECUTIVE_TOOL_ERRORS in a row ends that as a real failure. A tool that could not be
// *asked* (Tool Service unreachable / RPC transport error) or whose answer needs an approval
// Engine can't give stays a hard TaskError::Failed.

use common::proto::tools::v1::{ExecuteRequest, ListToolsRequest, Tool, ToolServiceClient};
use connectrpc::Protocol;
use connectrpc::client::{ClientConfig, HttpClient};
use engine_core::{TaskError, TaskExecutor};
use serde_json::Value;
use std::time::Duration;

const CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// How many tool calls in a row may come back as errors before the execution gives up. A tool
/// error is an observation the model can recover from (retry with fixed arguments, pick another
/// tool, answer with what it has), but an agent that keeps hitting the same wall must not loop
/// until `max_iterations`/budget — it fails here, with the last error in the message.
pub const MAX_CONSECUTIVE_TOOL_ERRORS: usize = 3;

/// How many of the most recent `state["tool_result"]` entries are error observations. The array
/// is agent_graph's Append-reduced tool log (see `engine-core/src/builder.rs`), so its tail *is*
/// the consecutive-error run: a successful call appends a plain result and resets the count.
fn consecutive_tool_errors(state: &Value) -> usize {
    state
        .get("tool_result")
        .and_then(Value::as_array)
        .map(|results| {
            results
                .iter()
                .rev()
                .take_while(|result| result.get("error").is_some())
                .count()
        })
        .unwrap_or(0)
}

/// One recoverable tool error as this executor's output: `{"error": msg}`, which the tool node's
/// Append reducer lands in `state["tool_result"]` and `build_prompt` renders back to the model —
/// unless this is the `MAX_CONSECUTIVE_TOOL_ERRORS`'th in a row, which fails the execution.
fn recoverable_error(state: &Value, message: &str) -> Result<Value, TaskError> {
    if consecutive_tool_errors(state) + 1 >= MAX_CONSECUTIVE_TOOL_ERRORS {
        return Err(TaskError::Failed(format!(
            "{MAX_CONSECUTIVE_TOOL_ERRORS} consecutive tool errors, giving up; last error: {message}"
        )));
    }
    Ok(serde_json::json!({ "error": message }))
}

pub struct ToolTaskExecutor {
    client: ToolServiceClient<HttpClient>,
}

impl ToolTaskExecutor {
    pub fn new(tool_service_url: &str) -> Result<Self, String> {
        let target = tool_service_url
            .parse()
            .map_err(|e| format!("could not parse TOOL_SERVICE_URL {tool_service_url:?}: {e}"))?;
        Ok(Self {
            client: ToolServiceClient::new(
                HttpClient::plaintext_http2_only(),
                ClientConfig::new(target)
                    .with_protocol(Protocol::Grpc)
                    .with_default_timeout(CALL_TIMEOUT)
                    .proto(),
            ),
        })
    }

    /// The catalog `Dispatcher` (Task 14) injects into an LLM node's `config` before a
    /// tool-calling turn — a plain RPC, not part of the `TaskExecutor` port.
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
        debug_assert_eq!(kind, "tool");
        let (slug, args) = tool_call(config, state)?;

        let response = self
            .client
            .execute(ExecuteRequest {
                slug,
                input_json: args.to_string(),
                idempotency_key: idempotency_key.to_owned(),
                ..Default::default()
            })
            .await
            .map_err(|e| TaskError::Failed(e.to_string()))?
            .into_owned();

        match response.status.as_str() {
            "ok" => serde_json::from_str(&response.output_json)
                .map_err(|e| TaskError::Failed(e.to_string())),
            // Not recoverable: nothing the model can say makes an unapproved tool run, so this
            // stays a hard failure, as does an unreachable Tool Service (the `?` above).
            "requires_approval" => Err(TaskError::Failed(
                "tool requires approval — not yet wired in Engine".to_owned(),
            )),
            // Recoverable: the tool itself said no (bad arguments, a site that blocks bots, a
            // timeout). Hand that back to the model as an observation instead of crashing.
            _ => recoverable_error(state, &response.error_message),
        }
    }
}

/// `(slug, args)` for this call, resolved per the two conventions documented at the top of this
/// file: a config-fixed `tool_slug` (rag_graph) wins; otherwise the LLM-decided
/// `state["llm"]["tool_call"]` (agent_graph).
fn tool_call(config: &Value, state: &Value) -> Result<(String, Value), TaskError> {
    if let Some(slug) = config.get("tool_slug").and_then(Value::as_str) {
        let query = state
            .get("question")
            .and_then(Value::as_str)
            .unwrap_or_default();
        return Ok((slug.to_owned(), serde_json::json!({"query": query})));
    }
    let call = state.pointer("/llm/tool_call").ok_or_else(|| {
        TaskError::Failed("no tool_call at state[\"llm\"][\"tool_call\"]".to_owned())
    })?;
    let slug = call
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| TaskError::Failed("tool_call.name is missing".to_owned()))?
        .to_owned();
    let args = call
        .get("args")
        .cloned()
        .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
    Ok((slug, args))
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
        status: String,
        output_json: String,
        error_message: String,
    }

    impl FakeToolService {
        fn ok(output_json: &str) -> Self {
            Self {
                received: Mutex::new(Vec::new()),
                status: "ok".to_owned(),
                output_json: output_json.to_owned(),
                error_message: String::new(),
            }
        }

        fn failing(status: &str, error_message: &str) -> Self {
            Self {
                received: Mutex::new(Vec::new()),
                status: status.to_owned(),
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
                status: self.status.clone(),
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
    async fn execute_maps_requires_approval_to_a_clear_error() {
        let fake = Arc::new(FakeToolService::failing("requires_approval", ""));
        let url = serve(fake).await;
        let executor = ToolTaskExecutor::new(&url).expect("client");
        let state = serde_json::json!({"llm": {"tool_call": {"name": "send_email", "args": {}}}});

        let error = executor
            .execute("tool", &serde_json::json!({}), &state, "e:tool:0")
            .await
            .unwrap_err();

        assert!(matches!(error, TaskError::Failed(msg) if msg.contains("requires approval")));
    }

    /// A tool that ran and said no is an observation, not a crash: the output the graph appends
    /// to state["tool_result"] is {"error": ...}, which the next llm node renders back to the
    /// model — the accuweather/kb_search symptoms that used to fail the whole execution.
    #[tokio::test]
    async fn a_tool_error_status_becomes_a_recoverable_error_observation() {
        let fake = Arc::new(FakeToolService::failing(
            "error",
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
            "error",
            "error sending request for url (https://www.accuweather.com/)",
        ));
        let url = serve(fake).await;
        let executor = ToolTaskExecutor::new(&url).expect("client");
        // Two error observations already in the log — this call is the third in a row.
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
        let fake = Arc::new(FakeToolService::failing("error", "still broken"));
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
