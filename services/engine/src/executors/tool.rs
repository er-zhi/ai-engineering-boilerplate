// TaskExecutor for kind="tool": calls Tool Service's Execute. Two graphs give it the call two
// different ways: agent_graph's tool node has empty config and an LLM decides the call, so it
// lands at state["llm"]["tool_call"] (see engine-core/src/builder.rs's doc comment on that
// node); rag_graph's tool node IS the entry point — no llm node has run yet, so there is no
// state["llm"] to read — and instead fixes its slug in config["tool_slug"], searching
// state["question"] (StartExecution's own initial state). A config-given tool_slug wins when
// present; otherwise this falls back to the state["llm"]["tool_call"] convention.

use common::proto::tools::v1::{ExecuteRequest, ListToolsRequest, Tool, ToolServiceClient};
use connectrpc::Protocol;
use connectrpc::client::{ClientConfig, HttpClient};
use engine_core::{TaskError, TaskExecutor};
use serde_json::Value;
use std::time::Duration;

const CALL_TIMEOUT: Duration = Duration::from_secs(60);

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
            "requires_approval" => Err(TaskError::Failed(
                "tool requires approval — not yet wired in Engine".to_owned(),
            )),
            _ => Err(TaskError::Failed(response.error_message)),
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
        let fake = Arc::new(FakeToolService {
            received: Mutex::new(Vec::new()),
            status: "ok".to_owned(),
            output_json: r#"{"results": []}"#.to_owned(),
        });
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
        let fake = Arc::new(FakeToolService {
            received: Mutex::new(Vec::new()),
            status: "ok".to_owned(),
            output_json: r#"{"results": []}"#.to_owned(),
        });
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
        let fake = Arc::new(FakeToolService {
            received: Mutex::new(Vec::new()),
            status: "requires_approval".to_owned(),
            output_json: String::new(),
        });
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
    async fn list_tools_returns_the_catalog() {
        let fake = Arc::new(FakeToolService {
            received: Mutex::new(Vec::new()),
            status: "ok".to_owned(),
            output_json: "{}".to_owned(),
        });
        let url = serve(fake).await;
        let executor = ToolTaskExecutor::new(&url).expect("client");

        let tools = executor.list_tools().await.expect("list");

        assert_eq!(tools[0].slug, "web_search");
    }
}
