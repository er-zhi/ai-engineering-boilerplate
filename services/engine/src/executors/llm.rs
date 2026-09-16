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
A tool result shown as an ERROR means that call failed, not that the task is over: fix the
arguments and call the tool again, call a different tool that can answer the question, or reply
with the best answer you can give from what you already have. Do not repeat a call that has
already failed the same way.
"#;
/// Appended after the `available_tools` list (so the model reads the rule with the catalog in
/// front of it, not before it). Without this a model asked for today's weather happily replies
/// "as an AI I don't have real-time data" and never calls a tool at all — the loop is wired, the
/// catalog is injected, and the model still answers from training data.
const TOOL_USE_POLICY: &str = r#"
You have live access to these tools and MUST use them rather than answering from memory whenever
the question is time-sensitive (today, current, latest, now, weather, news, prices, scores),
concerns a specific real-world fact you are not certain of, or asks about this system's own
knowledge base or documents. In those cases call web_search (for anything on the public web) or
kb_search (for the knowledge base) FIRST and answer from the result.
Never reply that you lack real-time data, cannot browse, or that your knowledge has a cutoff
while a search tool is listed above — call it instead. Every tool listed above is connected and
working: never decline because you are unsure a tool is available or operational, and never tell
the user to go look somewhere else. Call it — if it fails you will be shown the error.
Only answer directly when the question needs no outside information, or when the tools have
already given you what you need.
Keep going until you have the actual value. If the search snippets do not already contain the
concrete answer, call web_fetch on the most relevant result — prefer plain, official, text-first
pages (for example weather.gov or a plain-text weather endpoint) over JavaScript-heavy consumer
sites, and if one fetch fails, try the next result instead of giving up.
Then answer the question itself, with the real data in it — the numbers, dates and names you
found — and cite the source URL. Never reply with a list of sites to check, a "you can find it
at ..." pointer, or a "typically it is ..." guess when a tool can get the real value.
Your reply is either a tool_call or the finished answer — never a description of what you are
about to do. Do not say "I will search", "I will fetch a better source" or "let me check": if
that is your next step, make the tool_call for it in this very response instead.
"#;
/// Appended to the system prompt for the one extra turn `execute` takes when the model described
/// its next step, or declined, instead of calling a tool.
const TOOL_NUDGE: &str = r#"
Your previous response described what you were going to do, or declined for lack of access,
instead of using a tool. Do it now: respond with the tool_call itself. Every tool listed above is
connected and working — call it; if it fails you will be shown the error and can try another. If
you truly cannot make that call, answer the question with what you already have — do not describe
another plan and do not decline again.
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
    if let Some(tools) = config.get("available_tools").and_then(Value::as_array) {
        system.push_str("\n\nAvailable tools:\n");
        for tool in tools {
            let name = tool.get("name").and_then(Value::as_str).unwrap_or_default();
            let description = tool
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default();
            system.push_str(&format!("- {name}: {description}\n"));
        }
    }
    if config.get("tool_calling").and_then(Value::as_bool) == Some(true) {
        system.push_str(TOOL_USE_POLICY);
    }

    let question = state
        .get("question")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut user = question.to_owned();
    if let Some(results) = state.get("tool_result").and_then(Value::as_array) {
        for (index, result) in results.iter().enumerate() {
            // A {"error": msg} entry is ToolTaskExecutor's recoverable-error observation (see
            // services/engine/src/executors/tool.rs). Label it so the model reads it as a failed
            // call it may recover from, not as data the tool returned.
            match result.get("error").and_then(Value::as_str) {
                Some(error) => user.push_str(&format!("\n\nTool result {index} — ERROR: {error}")),
                None => user.push_str(&format!("\n\nTool result {index}: {result}")),
            }
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

        let output = self.complete_with_retries(&request).await?;

        // The model answered with an intention ("I will search for a better source...") and no
        // tool_call. The graph can only read that as a final answer — the llm → end edge fires on
        // a null tool_call — so the execution completes with a narration instead of an answer.
        // Nudge once, here, where it costs one call rather than a whole loop iteration.
        if config.get("tool_calling").and_then(Value::as_bool) == Some(true)
            && output["tool_call"].is_null()
            && needs_tool_nudge(output["reply"].as_str().unwrap_or_default())
        {
            tracing::info!("llm answered without using a tool it should have used, nudging once");
            let mut nudged = request.clone();
            nudged.system_prompt.push_str(TOOL_NUDGE);
            let second = self.complete_with_retries(&nudged).await?;
            return Ok(merge_usage(&output, second));
        }
        Ok(output)
    }
}

impl LlmTaskExecutor {
    async fn complete_with_retries(&self, request: &CompleteRequest) -> Result<Value, TaskError> {
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

/// The nudged turn's output, carrying both turns' token usage — the discarded first call was
/// really spent, and `step()` charges the budget from whatever this node returns.
fn merge_usage(first: &Value, mut second: Value) -> Value {
    let field = |value: &Value, name: &str| value["usage"][name].as_u64().unwrap_or(0);
    let merged = json!({
        "tokens_in": field(first, "tokens_in") + field(&second, "tokens_in"),
        "tokens_out": field(first, "tokens_out") + field(&second, "tokens_out"),
    });
    if let Some(object) = second.as_object_mut() {
        object.insert("usage".to_owned(), merged);
    }
    second
}

/// Whether a tool-less reply is a non-answer that one nudge can turn into a real one. Two kinds
/// have been seen live, both ending an execution with no answer in it:
///   - a *narration*: the model describes the call it is about to make instead of making it
///     ("I will search for a more reliable weather source");
///   - a *refusal*: the model declines while the tools it needs are listed right there ("I do not
///     have access to a search tool", "I cannot confirm its operational status").
///
/// Deliberately narrow, matching the way these replies talk about themselves — a real answer that
/// merely mentions searching ("the search results say it is 62F") is untouched.
#[must_use]
pub fn needs_tool_nudge(reply: &str) -> bool {
    const NARRATED_INTENTS: [&str; 10] = [
        "i will search",
        "i will fetch",
        "i will look",
        "i will check",
        "i will try",
        "i'll search",
        "i'll fetch",
        "i'll look",
        "i'll check",
        "let me search",
    ];
    const REFUSALS: [&str; 8] = [
        "i do not have access",
        "i don't have access",
        "i cannot confirm",
        "i can't confirm",
        "i cannot reliably provide",
        "i do not have real-time",
        "i don't have real-time",
        "i am unable to search",
    ];
    let lowered = reply.to_lowercase();
    NARRATED_INTENTS
        .iter()
        .chain(REFUSALS.iter())
        .any(|phrase| lowered.contains(phrase))
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
    fn build_prompt_renders_available_tools_into_the_system_prompt() {
        let config =
            json!({"available_tools": [{"name": "web_search", "description": "search the web"}]});
        let (system, _) = build_prompt(&config, &json!({}));
        assert!(system.contains("web_search"));
        assert!(system.contains("search the web"));
    }

    #[test]
    fn build_prompt_folds_prior_tool_results_into_the_user_prompt() {
        let state = json!({"question": "weather in SF?", "tool_result": ["62F, fog"]});
        let (_, user) = build_prompt(&json!({}), &state);
        assert!(user.contains("weather in SF?"));
        assert!(user.contains("62F, fog"));
    }

    /// A tool-calling turn must tell the model it has live access and may not fall back on "I'm
    /// an AI without real-time data" — the catalog alone did not get it to call web_search.
    #[test]
    fn build_prompt_requires_a_search_before_answering_time_sensitive_questions() {
        let config = json!({
            "tool_calling": true,
            "available_tools": [{"name": "web_search", "description": "search the web"}],
        });
        let (system, _) = build_prompt(&config, &json!({"question": "weather in SF today?"}));

        // The policy comes after the catalog, so the model reads the rule with the tools in view.
        let tools_at = system.find("web_search").expect("catalog rendered");
        let policy_at = system.find("MUST use them").expect("policy rendered");
        assert!(policy_at > tools_at, "{system}");
        assert!(system.contains("time-sensitive"), "{system}");
        assert!(
            system.contains("Never reply that you lack real-time data"),
            "{system}"
        );
    }

    /// Searching isn't the goal — the answer is. The policy must push the model from snippets to
    /// web_fetch and then to a concrete, cited answer, never to "check one of these sites".
    #[test]
    fn build_prompt_requires_drilling_down_to_a_concrete_cited_answer() {
        let config = json!({"tool_calling": true});
        let (system, _) = build_prompt(&config, &json!({}));

        assert!(
            system.contains("call web_fetch on the most relevant result"),
            "{system}"
        );
        assert!(
            system.contains("if one fetch fails, try the next result"),
            "{system}"
        );
        assert!(system.contains("cite the source URL"), "{system}");
        assert!(
            system.contains("Never reply with a list of sites to check"),
            "{system}"
        );
        assert!(
            system.contains("never a description of what you are"),
            "{system}"
        );
        assert!(
            system.contains("never decline because you are unsure a tool is available"),
            "{system}"
        );
    }

    #[test]
    fn build_prompt_without_tool_calling_carries_no_tool_use_policy() {
        let (system, _) = build_prompt(&json!({}), &json!({}));
        assert!(!system.contains("MUST use them"), "{system}");
    }

    #[test]
    fn build_prompt_renders_a_tool_error_observation_so_the_model_can_recover() {
        let state = json!({
            "question": "weather in SF?",
            "tool_result": [{"error": "error sending request for url (https://www.accuweather.com/)"}],
        });
        let (system, user) = build_prompt(&json!({"tool_calling": true}), &state);
        assert!(user.contains("ERROR"), "{user}");
        assert!(user.contains("accuweather.com"), "{user}");
        // ...and the contract tells it what it may do about that.
        assert!(system.contains("call a different tool"), "{system}");
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
        /// What the second and later calls answer, when the test needs the two turns to differ.
        then_reply: Option<String>,
    }

    impl FakeLlmRouter {
        fn always(reply: &str) -> Self {
            Self {
                received: Mutex::new(Vec::new()),
                reply: reply.to_owned(),
                then_reply: None,
            }
        }

        fn then(reply: &str, then_reply: &str) -> Self {
            Self {
                received: Mutex::new(Vec::new()),
                reply: reply.to_owned(),
                then_reply: Some(then_reply.to_owned()),
            }
        }

        fn calls(&self) -> usize {
            self.received.lock().expect("lock").len()
        }
    }

    #[allow(refining_impl_trait)]
    impl LlmRouterService for FakeLlmRouter {
        async fn complete(
            &self,
            _ctx: RequestContext,
            request: ServiceRequest<'_, CompleteRequest>,
        ) -> ServiceResult<CompleteResponse> {
            let first = {
                let mut received = self.received.lock().expect("lock");
                received.push(request.to_owned_message());
                received.len() == 1
            };
            let content = match (&self.then_reply, first) {
                (Some(then), false) => then.clone(),
                _ => self.reply.clone(),
            };
            Response::ok(CompleteResponse {
                content,
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
        let fake = Arc::new(FakeLlmRouter::always(
            r#"{"tool_call": {"name": "web_search", "args": {"query": "sf weather"}}}"#,
        ));
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

    #[test]
    fn a_narration_or_a_refusal_is_recognised_but_a_real_answer_is_not() {
        assert!(needs_tool_nudge(
            "I do not see the actual weather data in the fetch results. I will search for a more \
             reliable weather source for San Francisco today."
        ));
        assert!(needs_tool_nudge("Let me search for that."));
        assert!(needs_tool_nudge("I'll check the forecast page."));
        assert!(!needs_tool_nudge(
            "The search results say it is 62F and foggy in San Francisco today."
        ));
        assert!(!needs_tool_nudge(""));
        // The live kb_search refusal: tools listed, model declines anyway.
        assert!(needs_tool_nudge(
            "I do not have access to a search tool for this specific project's knowledge base. \
             The kb_search tool is listed but I cannot confirm its operational status."
        ));
        assert!(!needs_tool_nudge(
            "Claude Academy offers three courses: Prompting, Agents, and Evals."
        ));
    }

    #[tokio::test]
    async fn a_refusal_while_tools_are_listed_is_nudged_into_the_call() {
        let fake = Arc::new(FakeLlmRouter::then(
            r#"{"tool_call": null, "reply": "I do not have access to the knowledge base."}"#,
            r#"{"tool_call": {"name": "kb_search", "args": {"query": "Claude Academy courses"}}}"#,
        ));
        let url = serve(Arc::clone(&fake)).await;
        let executor = LlmTaskExecutor::new(&url).expect("client");

        let output = executor
            .execute(
                "llm",
                &json!({"tool_calling": true}),
                &json!({"question": "which Claude Academy courses exist?"}),
                "e:llm:0",
            )
            .await
            .expect("execute");

        assert_eq!(output["tool_call"]["name"], json!("kb_search"));
        assert_eq!(fake.calls(), 2);
    }

    /// The model announcing its next step used to end the execution: the graph reads a null
    /// tool_call as "this is the final answer". One nudge, inside the same node, turns the
    /// narration into the call it was describing.
    #[tokio::test]
    async fn a_narrated_intent_is_nudged_once_into_the_tool_call_it_described() {
        let fake = Arc::new(FakeLlmRouter::then(
            r#"{"tool_call": null, "reply": "I will search for a more reliable weather source."}"#,
            r#"{"tool_call": {"name": "web_search", "args": {"query": "san francisco weather today"}}}"#,
        ));
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
        assert_eq!(fake.calls(), 2);
        let received = fake.received.lock().expect("lock");
        assert!(received[1].system_prompt.contains("Do it now"));
        assert!(received[1].system_prompt.contains("connected and working"));
        // Both turns were really paid for, so both are charged to the budget.
        assert_eq!(output["usage"]["tokens_in"], json!(FAKE_TOKENS_IN * 2));
    }

    #[tokio::test]
    async fn a_real_answer_is_never_nudged() {
        let fake = Arc::new(FakeLlmRouter::always(
            r#"{"tool_call": null, "reply": "It is 62F and foggy in San Francisco."}"#,
        ));
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

        assert_eq!(
            output["reply"],
            json!("It is 62F and foggy in San Francisco.")
        );
        assert_eq!(fake.calls(), 1);
    }

    /// The nudge is bounded at one: a model that narrates twice is not retried forever.
    #[tokio::test]
    async fn a_model_that_narrates_twice_is_not_nudged_again() {
        let fake = Arc::new(FakeLlmRouter::always(
            r#"{"tool_call": null, "reply": "I will search for a better source."}"#,
        ));
        let url = serve(Arc::clone(&fake)).await;
        let executor = LlmTaskExecutor::new(&url).expect("client");

        executor
            .execute(
                "llm",
                &json!({"tool_calling": true}),
                &json!({"question": "weather in SF?"}),
                "e:llm:0",
            )
            .await
            .expect("execute");

        assert_eq!(fake.calls(), 2);
    }

    #[tokio::test]
    async fn execute_reports_the_routers_token_usage_for_the_budget_to_charge() {
        let fake = Arc::new(FakeLlmRouter::always("Four."));
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
        let fake = Arc::new(FakeLlmRouter::always("Four."));
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
