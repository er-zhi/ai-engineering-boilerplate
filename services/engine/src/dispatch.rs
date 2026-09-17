// Routes a node kind to its TaskExecutor and injects the tool catalog.

use std::time::{Duration, Instant};

use engine_core::{TaskError, TaskExecutor};
use serde_json::Value;
use tokio::sync::Mutex;

use crate::executors::llm::{CatalogEntry, LlmNodeConfig, LlmTaskExecutor};
use crate::executors::tool::ToolTaskExecutor;

const CATALOG_TTL: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeKind {
    Llm,
    Tool,
}

impl NodeKind {
    pub fn parse(kind: &str) -> Result<Self, TaskError> {
        match kind {
            "llm" => Ok(Self::Llm),
            "tool" => Ok(Self::Tool),
            other => Err(TaskError::Failed(format!(
                "no TaskExecutor wired for kind {other:?}"
            ))),
        }
    }
}

struct CachedCatalog {
    tools: Vec<CatalogEntry>,
    fetched_at: Instant,
}

pub struct Dispatcher {
    pub llm: LlmTaskExecutor,
    pub tool: ToolTaskExecutor,
    catalog: Mutex<Option<CachedCatalog>>,
}

impl Dispatcher {
    #[must_use]
    pub fn new(llm: LlmTaskExecutor, tool: ToolTaskExecutor) -> Self {
        Self {
            llm,
            tool,
            catalog: Mutex::new(None),
        }
    }

    async fn available_tools(&self) -> Result<Vec<CatalogEntry>, TaskError> {
        let mut cached = self.catalog.lock().await;
        if let Some(catalog) = cached.as_ref()
            && catalog.fetched_at.elapsed() < CATALOG_TTL
        {
            return Ok(catalog.tools.clone());
        }
        let tools: Vec<CatalogEntry> = self
            .tool
            .list_tools()
            .await
            .map_err(TaskError::Failed)?
            .into_iter()
            .map(|tool| CatalogEntry {
                name: tool.slug,
                description: tool.description,
            })
            .collect();
        *cached = Some(CachedCatalog {
            tools: tools.clone(),
            fetched_at: Instant::now(),
        });
        Ok(tools)
    }

    async fn llm_config_with_catalog(&self, config: LlmNodeConfig) -> Result<Value, TaskError> {
        Ok(LlmNodeConfig {
            available_tools: self.available_tools().await?,
            ..config
        }
        .to_json())
    }
}

impl TaskExecutor for Dispatcher {
    async fn execute(
        &self,
        kind: &str,
        config: &Value,
        state: &Value,
        idempotency_key: &str,
    ) -> Result<Value, TaskError> {
        match NodeKind::parse(kind)? {
            NodeKind::Tool => {
                self.tool
                    .execute(kind, config, state, idempotency_key)
                    .await
            }
            NodeKind::Llm => {
                let parsed = LlmNodeConfig::parse(config)?;
                let config = if parsed.tool_calling {
                    self.llm_config_with_catalog(parsed).await?
                } else {
                    config.clone()
                };
                self.llm
                    .execute(kind, &config, state, idempotency_key)
                    .await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executors::llm::LlmTaskExecutor;
    use crate::executors::tool::ToolTaskExecutor;
    use buffa::EnumValue;
    use common::proto::llm_router::v1::{
        CompleteRequest, CompleteResponse, DescribeTiersRequest, DescribeTiersResponse,
        LlmRouterService,
    };
    use common::proto::tools::v1::{
        ActivateToolRequest, ActivateToolResponse, CreateToolRequest, CreateToolResponse,
        ExecuteRequest, ExecuteResponse, ExecuteStatus, ListToolsRequest, ListToolsResponse, Tool,
        ToolService, ValidateToolRequest, ValidateToolResponse,
    };
    use connectrpc::{
        RequestContext, Response, Router as ConnectRouter, ServiceRequest, ServiceResult,
    };
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
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

    #[derive(Default)]
    struct FakeTool {
        list_tools_calls: AtomicUsize,
    }

    #[allow(refining_impl_trait)]
    impl ToolService for FakeTool {
        async fn execute(
            &self,
            _ctx: RequestContext,
            _request: ServiceRequest<'_, ExecuteRequest>,
        ) -> ServiceResult<ExecuteResponse> {
            Response::ok(ExecuteResponse {
                status: EnumValue::Known(ExecuteStatus::Ok),
                output_json: "{}".to_owned(),
                ..Default::default()
            })
        }
        async fn list_tools(
            &self,
            _ctx: RequestContext,
            _request: ServiceRequest<'_, ListToolsRequest>,
        ) -> ServiceResult<ListToolsResponse> {
            self.list_tools_calls.fetch_add(1, Ordering::SeqCst);
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
        let tool_url = serve_tool(Arc::new(FakeTool::default())).await;
        let llm_url = serve_llm(Arc::new(FakeLlm {
            received_config_tools: Mutex::new(None),
        }))
        .await;
        let dispatcher = Dispatcher::new(
            LlmTaskExecutor::new(&llm_url).expect("llm client"),
            ToolTaskExecutor::new(&tool_url).expect("tool client"),
        );
        let state = json!({"llm": {"tool_call": {"name": "web_search", "args": {}}}});

        let output = dispatcher
            .execute("tool", &json!({}), &state, "e:t:0")
            .await
            .expect("execute");

        assert_eq!(output, json!({}));
    }

    #[tokio::test]
    async fn llm_kind_with_tool_calling_gets_the_catalog_injected_into_its_system_prompt() {
        let tool_url = serve_tool(Arc::new(FakeTool::default())).await;
        let llm = Arc::new(FakeLlm {
            received_config_tools: Mutex::new(None),
        });
        let llm_url = serve_llm(Arc::clone(&llm)).await;
        let dispatcher = Dispatcher::new(
            LlmTaskExecutor::new(&llm_url).expect("llm client"),
            ToolTaskExecutor::new(&tool_url).expect("tool client"),
        );
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
        let llm = Arc::new(FakeLlm {
            received_config_tools: Mutex::new(None),
        });
        let llm_url = serve_llm(Arc::clone(&llm)).await;
        let dispatcher = Dispatcher::new(
            LlmTaskExecutor::new(&llm_url).expect("llm client"),
            ToolTaskExecutor::new("http://127.0.0.1:1").expect("tool client"),
        );

        let output = dispatcher
            .execute("llm", &json!({}), &json!({}), "e:l:0")
            .await
            .expect("execute");

        assert_eq!(output["reply"], json!("ok"));
    }

    #[tokio::test]
    async fn the_tool_catalog_is_fetched_once_and_reused_across_tool_calling_turns() {
        let tool = Arc::new(FakeTool::default());
        let tool_url = serve_tool(Arc::clone(&tool)).await;
        let llm_url = serve_llm(Arc::new(FakeLlm {
            received_config_tools: Mutex::new(None),
        }))
        .await;
        let dispatcher = Dispatcher::new(
            LlmTaskExecutor::new(&llm_url).expect("llm client"),
            ToolTaskExecutor::new(&tool_url).expect("tool client"),
        );
        let config = json!({"tool_calling": true});

        for turn in 0..3 {
            dispatcher
                .execute("llm", &config, &json!({}), &format!("e:l:{turn}"))
                .await
                .expect("execute");
        }

        assert_eq!(
            tool.list_tools_calls.load(Ordering::SeqCst),
            1,
            "every turn after the first reads the cached catalog"
        );
    }

    #[test]
    fn an_unwired_node_kind_is_rejected_by_name() {
        assert_eq!(NodeKind::parse("llm"), Ok(NodeKind::Llm));
        assert_eq!(NodeKind::parse("tool"), Ok(NodeKind::Tool));
        assert!(matches!(NodeKind::parse("noop"), Err(TaskError::Failed(_))));
    }
}
