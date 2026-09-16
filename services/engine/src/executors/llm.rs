// TaskExecutor for kind="llm": calls llm-router, mapping its plain-text CompleteResponse into
// the {tool_call, reply} shape agent_graph's edges check (see the module doc above for why),
// plus a {usage: {tokens_in, tokens_out}} block that step() charges the execution's Budget from.

use std::time::Duration;

use buffa::EnumValue;
use common::proto::llm_router::v1::{
    CompleteRequest, CompleteResponse, LlmRouterServiceClient, QualityTier, ResponseFormat,
    Sampling,
};
use connectrpc::Protocol;
use connectrpc::client::{ClientConfig, HttpClient};
use engine_core::{TaskError, TaskExecutor};
use serde_json::{Value, json};

const DEFAULT_SYSTEM_PROMPT: &str = "You are a helpful assistant.";
const TOOL_CALLING_INSTRUCTIONS: &str = r#"
If you need a tool, respond with exactly this JSON and nothing else:
{"tool_call": {"name": "<tool slug>", "args": {...}}}
Otherwise, respond with exactly this JSON and nothing else:
{"tool_call": null, "reply": "<your answer>"}
"#;
const CALL_TIMEOUT: Duration = Duration::from_secs(60);
const RETRY_ATTEMPTS: u32 = 3;
const RETRY_BASE_DELAY: Duration = Duration::from_millis(200);

/// Builds `(system_prompt, user_prompt)` from a Task's `config` and the execution's current
/// `state`. `config.system_prompt` overrides the default; `config.tool_calling: true` appends
/// the JSON-contract instructions. `state.question` is the base question; any
/// `state.tool_result` entries (an array — see Task 7's `agent_graph`) are rendered after it so
/// a later loop iteration sees what earlier tool calls returned.
#[must_use]
pub fn build_prompt(config: &Value, state: &Value) -> (String, String) {
    let mut system = config
        .get("system_prompt")
        .and_then(Value::as_str)
        .unwrap_or(DEFAULT_SYSTEM_PROMPT)
        .to_owned();
    if config.get("tool_calling").and_then(Value::as_bool) == Some(true) {
        system.push_str(TOOL_CALLING_INSTRUCTIONS);
    }

    let question = state
        .get("question")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut user = question.to_owned();
    if let Some(results) = state.get("tool_result").and_then(Value::as_array) {
        for (index, result) in results.iter().enumerate() {
            user.push_str(&format!("\n\nTool result {index}: {result}"));
        }
    }
    (system, user)
}

/// Parses a model's plain-text response into `{"tool_call": ..|null, "reply": ..|null}`. A
/// response that isn't the requested JSON shape is treated as a plain final answer — a model
/// that ignores the JSON-contract instructions should still produce a usable (if unstructured)
/// result rather than fail the whole execution.
#[must_use]
pub fn parse_llm_output(content: &str) -> Value {
    match serde_json::from_str::<Value>(content) {
        Ok(parsed) if parsed.get("tool_call").is_some() || parsed.get("reply").is_some() => {
            json!({
                "tool_call": parsed.get("tool_call").cloned().unwrap_or(Value::Null),
                "reply": parsed.get("reply").cloned().unwrap_or(Value::Null),
            })
        }
        _ => json!({"tool_call": Value::Null, "reply": content}),
    }
}

/// One successful `CompleteResponse` as this executor's output value: `parse_llm_output`'s
/// `{tool_call, reply}` shape plus a `usage` block. `step()` charges the execution's `Budget`
/// from that block — without it a completed Task only ever costs its one tool call, so the token
/// limit would never bind on the one node kind that actually spends tokens.
#[must_use]
fn response_to_output(response: &CompleteResponse) -> Value {
    let mut output = parse_llm_output(&response.content);
    if let Some(object) = output.as_object_mut() {
        object.insert(
            "usage".to_owned(),
            json!({"tokens_in": response.tokens_in, "tokens_out": response.tokens_out}),
        );
    }
    output
}

pub struct LlmTaskExecutor {
    client: LlmRouterServiceClient<HttpClient>,
}

impl LlmTaskExecutor {
    pub fn new(llm_router_url: &str) -> Result<Self, String> {
        let target = llm_router_url.parse().map_err(|error| {
            format!("could not parse LLM_ROUTER_URL {llm_router_url:?}: {error}")
        })?;
        Ok(Self {
            client: LlmRouterServiceClient::new(
                HttpClient::plaintext_http2_only(),
                ClientConfig::new(target)
                    .with_protocol(Protocol::Grpc)
                    .with_default_timeout(CALL_TIMEOUT)
                    .proto(),
            ),
        })
    }
}

impl TaskExecutor for LlmTaskExecutor {
    async fn execute(
        &self,
        kind: &str,
        config: &Value,
        state: &Value,
        idempotency_key: &str,
    ) -> Result<Value, TaskError> {
        debug_assert_eq!(kind, "llm");
        tracing::debug!(
            idempotency_key,
            "llm task executing (key logged, not enforced — no destructive side effect to dedupe, see the spec's Порты)"
        );
        let (system_prompt, user_prompt) = build_prompt(config, state);
        // When the graph asked for the JSON tool-call contract, also ask the provider for a JSON
        // object response — every served tier supports it (common::llm::RESPONSE_FORMATS), and
        // it makes parse_llm_output's happy path the common one instead of the fallback.
        let response_format = if config.get("tool_calling").and_then(Value::as_bool) == Some(true) {
            Some(EnumValue::Known(ResponseFormat::JsonObject))
        } else {
            None
        };
        let request = CompleteRequest {
            tier: EnumValue::Known(QualityTier::Medium),
            system_prompt,
            user_prompt,
            sampling: Sampling {
                response_format,
                ..Default::default()
            }
            .into(),
            ..Default::default()
        };

        let mut attempt = 0;
        loop {
            attempt += 1;
            match self.client.complete(request.clone()).await {
                Ok(response) => return Ok(response_to_output(&response.into_owned())),
                Err(error) if attempt < RETRY_ATTEMPTS => {
                    tracing::warn!(attempt, %error, "llm-router call failed, retrying");
                    tokio::time::sleep(RETRY_BASE_DELAY * attempt).await;
                }
                Err(error) => return Err(TaskError::Failed(error.to_string())),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_prompt_uses_the_default_system_prompt_when_config_has_none() {
        let (system, _) = build_prompt(&json!({}), &json!({}));
        assert_eq!(system, DEFAULT_SYSTEM_PROMPT);
    }

    #[test]
    fn build_prompt_appends_tool_instructions_when_requested() {
        let (system, _) = build_prompt(&json!({"tool_calling": true}), &json!({}));
        assert!(system.contains("tool_call"));
    }

    #[test]
    fn build_prompt_folds_prior_tool_results_into_the_user_prompt() {
        let state = json!({"question": "weather in SF?", "tool_result": ["62F, fog"]});
        let (_, user) = build_prompt(&json!({}), &state);
        assert!(user.contains("weather in SF?"));
        assert!(user.contains("62F, fog"));
    }

    #[test]
    fn parse_llm_output_reads_a_tool_call() {
        let output = parse_llm_output(r#"{"tool_call": {"name": "web_search", "args": {}}}"#);
        assert_eq!(output["tool_call"]["name"], json!("web_search"));
        assert_eq!(output["reply"], Value::Null);
    }

    #[test]
    fn parse_llm_output_reads_a_final_reply() {
        let output = parse_llm_output(r#"{"tool_call": null, "reply": "62F and foggy"}"#);
        assert_eq!(output["tool_call"], Value::Null);
        assert_eq!(output["reply"], json!("62F and foggy"));
    }

    #[test]
    fn parse_llm_output_falls_back_to_treating_non_json_content_as_the_reply() {
        let output = parse_llm_output("It's 62F and foggy in San Francisco.");
        assert_eq!(output["tool_call"], Value::Null);
        assert_eq!(
            output["reply"],
            json!("It's 62F and foggy in San Francisco.")
        );
    }

    use std::sync::{Arc, Mutex};

    use common::proto::llm_router::v1::{
        DescribeTiersRequest, DescribeTiersResponse, LlmRouterService,
    };
    use connectrpc::{
        RequestContext, Response, Router as ConnectRouter, ServiceRequest, ServiceResult,
    };

    const FAKE_TOKENS_IN: i32 = 17;
    const FAKE_TOKENS_OUT: i32 = 23;

    struct FakeLlmRouter {
        received: Mutex<Vec<CompleteRequest>>,
        reply: String,
    }

    #[allow(refining_impl_trait)]
    impl LlmRouterService for FakeLlmRouter {
        async fn complete(
            &self,
            _ctx: RequestContext,
            request: ServiceRequest<'_, CompleteRequest>,
        ) -> ServiceResult<CompleteResponse> {
            self.received
                .lock()
                .expect("lock")
                .push(request.to_owned_message());
            Response::ok(CompleteResponse {
                content: self.reply.clone(),
                tokens_in: FAKE_TOKENS_IN,
                tokens_out: FAKE_TOKENS_OUT,
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

    async fn serve(fake: Arc<FakeLlmRouter>) -> String {
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
    async fn execute_sends_the_built_prompt_and_parses_a_tool_call_reply() {
        let fake = Arc::new(FakeLlmRouter {
            received: Mutex::new(Vec::new()),
            reply: r#"{"tool_call": {"name": "web_search", "args": {"query": "sf weather"}}}"#
                .to_owned(),
        });
        let url = serve(Arc::clone(&fake)).await;
        let executor = LlmTaskExecutor::new(&url).expect("client");

        let output = executor
            .execute(
                "llm",
                &json!({"tool_calling": true}),
                &json!({"question": "weather in SF?"}),
                "e:llm:0",
            )
            .await
            .expect("execute");

        assert_eq!(output["tool_call"]["name"], json!("web_search"));
        let received = fake.received.lock().expect("lock");
        assert_eq!(received.len(), 1);
        assert_eq!(received[0].tier, EnumValue::Known(QualityTier::Medium));
        assert!(received[0].system_prompt.contains("tool_call"));
        assert_eq!(received[0].user_prompt, "weather in SF?");
        assert_eq!(
            received[0].sampling.response_format,
            Some(EnumValue::Known(ResponseFormat::JsonObject))
        );
    }

    #[tokio::test]
    async fn execute_reports_the_routers_token_usage_for_the_budget_to_charge() {
        let fake = Arc::new(FakeLlmRouter {
            received: Mutex::new(Vec::new()),
            reply: "Four.".to_owned(),
        });
        let url = serve(Arc::clone(&fake)).await;
        let executor = LlmTaskExecutor::new(&url).expect("client");

        let output = executor
            .execute("llm", &json!({}), &json!({"question": "2+2?"}), "e:llm:0")
            .await
            .expect("execute");

        assert_eq!(
            output["usage"],
            json!({"tokens_in": FAKE_TOKENS_IN, "tokens_out": FAKE_TOKENS_OUT})
        );
    }

    #[tokio::test]
    async fn execute_without_tool_calling_asks_for_no_response_format() {
        let fake = Arc::new(FakeLlmRouter {
            received: Mutex::new(Vec::new()),
            reply: "Four.".to_owned(),
        });
        let url = serve(Arc::clone(&fake)).await;
        let executor = LlmTaskExecutor::new(&url).expect("client");

        let output = executor
            .execute("llm", &json!({}), &json!({"question": "2+2?"}), "e:llm:0")
            .await
            .expect("execute");

        assert_eq!(output["reply"], json!("Four."));
        assert_eq!(
            fake.received.lock().expect("lock")[0]
                .sampling
                .response_format,
            None
        );
    }
}
