// The one place Task.kind strings map to a real TaskExecutor implementation. For "llm" nodes
// whose config asks for tool_calling, the Dispatcher fetches the tool catalog from Tool Service
// and folds it into the config it hands to LlmTaskExecutor — LlmTaskExecutor itself has no
// knowledge of Tool Service, it only reads whatever ends up in config["available_tools"].

use engine_core::{TaskError, TaskExecutor};
use serde_json::{Value, json};

use crate::executors::llm::LlmTaskExecutor;
use crate::executors::tool::ToolTaskExecutor;

pub struct Dispatcher {
    pub llm: LlmTaskExecutor,
    pub tool: ToolTaskExecutor,
}

impl TaskExecutor for Dispatcher {
    async fn execute(
        &self,
        kind: &str,
        config: &Value,
        state: &Value,
        idempotency_key: &str,
    ) -> Result<Value, TaskError> {
        match kind {
            "tool" => {
                self.tool
                    .execute(kind, config, state, idempotency_key)
                    .await
            }
            "llm" if config.get("tool_calling").and_then(Value::as_bool) == Some(true) => {
                let tools = self
                    .tool
                    .list_tools()
                    .await
                    .map_err(TaskError::Failed)?
                    .into_iter()
                    .map(|t| json!({"name": t.slug, "description": t.description}))
                    .collect::<Vec<_>>();
                let mut config = config.clone();
                if let Some(object) = config.as_object_mut() {
                    object.insert("available_tools".to_owned(), Value::Array(tools));
                }
                self.llm
                    .execute(kind, &config, state, idempotency_key)
                    .await
            }
            "llm" => self.llm.execute(kind, config, state, idempotency_key).await,
            other => Err(TaskError::Failed(format!(
                "no TaskExecutor wired for kind {other:?}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executors::llm::LlmTaskExecutor;
    use crate::executors::tool::ToolTaskExecutor;
    use common::proto::llm_router::v1::{
        CompleteRequest, CompleteResponse, DescribeTiersRequest, DescribeTiersResponse,
        LlmRouterService,
    };
    use common::proto::tools::v1::{
        ActivateToolRequest, ActivateToolResponse, CreateToolRequest, CreateToolResponse,
        ExecuteRequest, ExecuteResponse, ListToolsRequest, ListToolsResponse, Tool, ToolService,
        ValidateToolRequest, ValidateToolResponse,
    };
    use connectrpc::{
        RequestContext, Response, Router as ConnectRouter, ServiceRequest, ServiceResult,
    };
    use std::sync::{Arc, Mutex};

    struct FakeLlm {
        received_config_tools: Mutex<Option<Value>>,
    }

    #[allow(refining_impl_trait)]
    impl LlmRouterService for FakeLlm {
        async fn complete(
            &self,
            _ctx: RequestContext,
            request: ServiceRequest<'_, CompleteRequest>,
        ) -> ServiceResult<CompleteResponse> {
            let msg = request.to_owned_message();
            *self.received_config_tools.lock().expect("lock") = Some(json!(msg.system_prompt));
            Response::ok(CompleteResponse {
                content: r#"{"tool_call": null, "reply": "ok"}"#.to_owned(),
                ..Default::default()
            })
        }

        async fn describe_tiers(
            &self,
            _ctx: RequestContext,
            _request: ServiceRequest<'_, DescribeTiersRequest>,
        ) -> ServiceResult<DescribeTiersResponse> {
            Response::ok(DescribeTiersResponse::default())
        }
    }

    struct FakeTool;

    #[allow(refining_impl_trait)]
    impl ToolService for FakeTool {
        async fn execute(
            &self,
            _ctx: RequestContext,
            _request: ServiceRequest<'_, ExecuteRequest>,
        ) -> ServiceResult<ExecuteResponse> {
            Response::ok(ExecuteResponse {
                status: "ok".to_owned(),
                output_json: "{}".to_owned(),
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
                    description: "search the web".to_owned(),
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

    async fn serve_llm(fake: Arc<FakeLlm>) -> String {
        let connect = ConnectRouter::new().add_service(fake);
        let app = axum::Router::new().fallback_service(connect.into_axum_service());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("addr");
        tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
        format!("http://{address}")
    }

    async fn serve_tool(fake: Arc<FakeTool>) -> String {
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
    async fn tool_kind_routes_to_the_tool_executor() {
        let tool_url = serve_tool(Arc::new(FakeTool)).await;
        let llm_url = serve_llm(Arc::new(FakeLlm {
            received_config_tools: Mutex::new(None),
        }))
        .await;
        let dispatcher = Dispatcher {
            llm: LlmTaskExecutor::new(&llm_url).expect("llm client"),
            tool: ToolTaskExecutor::new(&tool_url).expect("tool client"),
        };
        let state = json!({"llm": {"tool_call": {"name": "web_search", "args": {}}}});

        let output = dispatcher
            .execute("tool", &json!({}), &state, "e:t:0")
            .await
            .expect("execute");

        assert_eq!(output, json!({}));
    }

    #[tokio::test]
    async fn llm_kind_with_tool_calling_gets_the_catalog_injected_into_its_system_prompt() {
        let tool_url = serve_tool(Arc::new(FakeTool)).await;
        let llm = Arc::new(FakeLlm {
            received_config_tools: Mutex::new(None),
        });
        let llm_url = serve_llm(Arc::clone(&llm)).await;
        let dispatcher = Dispatcher {
            llm: LlmTaskExecutor::new(&llm_url).expect("llm client"),
            tool: ToolTaskExecutor::new(&tool_url).expect("tool client"),
        };
        let config = json!({"tool_calling": true});

        dispatcher
            .execute("llm", &config, &json!({}), "e:l:0")
            .await
            .expect("execute");

        let received = llm
            .received_config_tools
            .lock()
            .expect("lock")
            .clone()
            .expect("called");
        assert!(received.as_str().unwrap().contains("web_search"));
        assert!(received.as_str().unwrap().contains("search the web"));
    }

    #[tokio::test]
    async fn llm_kind_without_tool_calling_never_calls_list_tools() {
        // A tool_url pointing nowhere real: if the Dispatcher called list_tools for a
        // non-tool-calling node, this would fail with a connection error instead of the
        // expected plain reply.
        let llm = Arc::new(FakeLlm {
            received_config_tools: Mutex::new(None),
        });
        let llm_url = serve_llm(Arc::clone(&llm)).await;
        let dispatcher = Dispatcher {
            llm: LlmTaskExecutor::new(&llm_url).expect("llm client"),
            tool: ToolTaskExecutor::new("http://127.0.0.1:1").expect("tool client"),
        };

        let output = dispatcher
            .execute("llm", &json!({}), &json!({}), "e:l:0")
            .await
            .expect("execute");

        assert_eq!(output["reply"], json!("ok"));
    }
}
