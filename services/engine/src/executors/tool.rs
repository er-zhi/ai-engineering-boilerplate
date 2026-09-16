// TaskExecutor for kind="tool": calls Tool Service's Execute, reading the call itself from
// state["llm"]["tool_call"] (agent_graph's own convention, not from `config` — see
// engine-core/src/builder.rs's doc comment on the "tool" node).

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
        _config: &Value,
        state: &Value,
        idempotency_key: &str,
    ) -> Result<Value, TaskError> {
        debug_assert_eq!(kind, "tool");
        let call = state.pointer("/llm/tool_call").ok_or_else(|| {
            TaskError::Failed("no tool_call at state[\"llm\"][\"tool_call\"]".to_owned())
        })?;
        let slug = call
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| TaskError::Failed("tool_call.name is missing".to_owned()))?;
        let args = call
            .get("args")
            .cloned()
            .unwrap_or_else(|| Value::Object(serde_json::Map::new()));

        let response = self
            .client
            .execute(ExecuteRequest {
                slug: slug.to_owned(),
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
