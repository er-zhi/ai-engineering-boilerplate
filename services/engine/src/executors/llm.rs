// Runs an llm node by calling llm-router and parsing its reply.

use std::sync::LazyLock;
use std::time::Duration;

use buffa::EnumValue;
use common::execution_input::ExecutionInput;
use common::proto::llm_router::v1::{
    CompleteRequest, CompleteResponse, LlmRouterServiceClient, QualityTier, ResponseFormat,
    Sampling,
};
use connectrpc::client::HttpClient;
use engine_core::{
    LlmOutput, TOOL_RESULT_ERROR_KEY, TOOL_RESULT_STATE_KEY, TaskError, TaskExecutor, ToolCall,
};
use serde_json::{Value, json};

use crate::dispatch::NodeKind;

const DEFAULT_SYSTEM_PROMPT: &str = "You are a helpful assistant.";
const TOOL_SLUG_PLACEHOLDER: &str = "<tool slug>";
const ARGUMENT_NAME_PLACEHOLDER: &str = "<argument name>";
const ARGUMENT_VALUE_PLACEHOLDER: &str = "<argument value>";
const ANSWER_PLACEHOLDER: &str = "<your answer>";
static TOOL_CALLING_INSTRUCTIONS: LazyLock<String> = LazyLock::new(|| {
    format!(
        r#"
If you need a tool, respond with exactly this JSON and nothing else:
{}
Otherwise, respond with exactly this JSON and nothing else:
{}
A tool result shown as an ERROR means that call failed, not that the task is over: fix the
arguments and call the tool again, call a different tool that can answer the question, or reply
with the best answer you can give from what you already have. Do not repeat a call that has
already failed the same way.
"#,
        shape_of(LlmOutput {
            tool_call: serde_json::to_value(ToolCall {
                name: TOOL_SLUG_PLACEHOLDER.to_owned(),
                args: json!({ARGUMENT_NAME_PLACEHOLDER: ARGUMENT_VALUE_PLACEHOLDER}),
            })
            .expect("a ToolCall always serializes"),
            reply: Value::Null,
        }),
        shape_of(LlmOutput {
            tool_call: Value::Null,
            reply: Value::String(ANSWER_PLACEHOLDER.to_owned()),
        }),
    )
});

const TOOL_USE_POLICY: &str = r#"
The tools listed above are live and connected. Use one rather than answering from memory
whenever you are not certain your answer is correct and current — anything that could have
changed since you were trained, any specific fact you would be guessing at, and anything about
this system's own data. Only answer directly when the question needs no outside information, or
when the tools have already given you what you need.
Never reply that you lack access, cannot browse, or that your knowledge has a cutoff while a
tool that could answer is listed above — call it instead. Never decline because you are unsure a
tool is available or working: call it, and if it fails you will be shown the error and can try
another. Never tell the user to go look somewhere else.
Keep going until you have the actual value. If one tool's result does not yet contain the
concrete answer, use what it gave you to call another — and when several candidates would each
answer the question, pass them together so whichever responds first wins, instead of spending a
turn per failure.
Your response is either a tool_call or the finished answer — never a description of what you are
about to do. If your next step is a tool call, make that call in this very response.
Every specific value you state — a number, a name, a date, a status — must appear in a tool result
you were shown in this conversation. Never fill one in from memory, and never carry one over from
what you knew before: a value you remember is out of date by definition, and a plausible wrong
value is worse than none. If a tool result does not contain what was asked, call another tool; if
nothing gives it to you, say you could not get it rather than supplying it yourself.
"#;
const TOOL_NUDGE: &str = r#"
You answered without using any tool. If a tool above could confirm or supply what you just said,
call it now: respond with the tool_call itself. Every tool listed is connected and working — if
it fails you will be shown the error and can try another. If no tool applies, or you truly
cannot make the call, answer the question with what you already have: do not describe a plan and
do not decline.
"#;
const FINAL_ANSWER_STYLE: &str = r#"
Final answer style, overriding any instinct to be thorough or helpful: your "reply" is read aloud.
Your reply is either the value you were asked for, or an admission that you could not get it.
There is no third option: naming a place the value could be found is not an answer, it is the
admission with extra words. If you do not have the value, the whole reply is a handful of words
saying so.
Answer only what was asked, in as few words as it takes — a fragment is better than a sentence,
and one line is the maximum. Give the value, not a write-up of it.
Never narrate your process, never list sources, never use markdown. Never add advice, suggestions,
next steps, alternatives, caveats or offers of further help: the user asked a question, not for
instructions.
A bare value, a fragment, or "Couldn't get it." are all complete answers. A sentence that opens
with "Based on my search" or closes by suggesting where else to look is not.
"#;
const CALL_TIMEOUT: Duration = Duration::from_secs(60);
const RETRY_ATTEMPTS: u32 = 3;
const RETRY_BASE_DELAY: Duration = Duration::from_millis(200);
const RENDERED_TOOL_RESULTS: usize = 4;
const MAX_TOOL_RESULT_CHARS: usize = 4_000;
const TRUNCATION_MARK: &str = "… (truncated)";

fn shape_of(output: LlmOutput) -> String {
    serde_json::to_string(&output).expect("an LlmOutput always serializes")
}

#[derive(Clone, Debug, Default, serde::Deserialize, serde::Serialize)]
#[serde(default)]
pub struct LlmNodeConfig {
    pub system_prompt: Option<String>,
    pub tool_calling: bool,
    pub available_tools: Vec<CatalogEntry>,
}

#[derive(Clone, Debug, Default, serde::Deserialize, serde::Serialize)]
#[serde(default)]
pub struct CatalogEntry {
    pub name: String,
    pub description: String,
}

impl LlmNodeConfig {
    pub fn parse(config: &Value) -> Result<Self, TaskError> {
        serde_json::from_value(config.clone())
            .map_err(|error| TaskError::Failed(format!("llm node config is not usable: {error}")))
    }

    #[must_use]
    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or_else(|_| {
            debug_assert!(false, "an LlmNodeConfig always serializes");
            Value::Object(serde_json::Map::new())
        })
    }
}

#[must_use]
pub fn build_prompt(config: &LlmNodeConfig, state: &Value) -> (String, String) {
    let mut system = config
        .system_prompt
        .clone()
        .unwrap_or_else(|| DEFAULT_SYSTEM_PROMPT.to_owned());
    if config.tool_calling {
        system.push_str(&TOOL_CALLING_INSTRUCTIONS);
    }
    if !config.available_tools.is_empty() {
        system.push_str("\n\nAvailable tools:\n");
        for tool in &config.available_tools {
            system.push_str(&format!("- {}: {}\n", tool.name, tool.description));
        }
    }
    if config.tool_calling {
        system.push_str(TOOL_USE_POLICY);
    }
    system.push_str(FINAL_ANSWER_STYLE);

    let mut user = ExecutionInput::in_state(state).question;
    for (index, result) in rendered_tool_results(state) {
        user.push_str(&render_tool_result(index, result));
    }
    (system, user)
}

fn rendered_tool_results(state: &Value) -> impl Iterator<Item = (usize, &Value)> {
    let results = state
        .get(TOOL_RESULT_STATE_KEY)
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    let first_rendered = results.len().saturating_sub(RENDERED_TOOL_RESULTS);
    results.iter().enumerate().skip(first_rendered)
}

fn render_tool_result(index: usize, result: &Value) -> String {
    match result.get(TOOL_RESULT_ERROR_KEY).and_then(Value::as_str) {
        Some(error) => format!("\n\nTool result {index} — ERROR: {}", truncated(error)),
        None => format!(
            "\n\nTool result {index}: {}",
            truncated(&result.to_string())
        ),
    }
}

fn truncated(text: &str) -> String {
    if text.chars().count() <= MAX_TOOL_RESULT_CHARS {
        return text.to_owned();
    }
    let kept: String = text.chars().take(MAX_TOOL_RESULT_CHARS).collect();
    format!("{kept}{TRUNCATION_MARK}")
}

#[must_use]
pub fn parse_llm_output(content: &str) -> Value {
    let parsed = serde_json::from_str::<LlmOutput>(content).unwrap_or_default();
    let output = if parsed.is_blank() {
        LlmOutput {
            tool_call: Value::Null,
            reply: Value::String(content.to_owned()),
        }
    } else {
        parsed
    };
    serde_json::to_value(output).expect("an LlmOutput always serializes")
}

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
        Ok(Self {
            client: crate::executors::grpc_client(
                "LLM_ROUTER_URL",
                llm_router_url,
                CALL_TIMEOUT,
                LlmRouterServiceClient::new,
            )?,
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
        debug_assert_eq!(NodeKind::parse(kind), Ok(NodeKind::Llm));
        tracing::debug!(
            idempotency_key,
            "llm task executing (key logged, not enforced — no destructive side effect to dedupe, see the spec's Порты)"
        );
        let config = LlmNodeConfig::parse(config)?;
        let (system_prompt, user_prompt) = build_prompt(&config, state);
        let response_format = config
            .tool_calling
            .then_some(EnumValue::Known(ResponseFormat::JsonObject));
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

        if answered_before_consulting_any_tool(&config, state, &output) {
            tracing::info!("llm answered before using any tool, nudging once");
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

#[must_use]
pub fn answered_before_consulting_any_tool(
    config: &LlmNodeConfig,
    state: &Value,
    output: &Value,
) -> bool {
    let written: LlmOutput = serde_json::from_value(output.clone()).unwrap_or_default();
    config.tool_calling
        && written.tool_call.is_null()
        && state
            .get(TOOL_RESULT_STATE_KEY)
            .and_then(Value::as_array)
            .is_none_or(Vec::is_empty)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_of(value: Value) -> LlmNodeConfig {
        LlmNodeConfig::parse(&value).expect("usable config")
    }

    #[test]
    fn build_prompt_uses_the_default_system_prompt_when_config_has_none() {
        let (system, _) = build_prompt(&config_of(json!({})), &json!({}));
        assert_eq!(
            system,
            format!("{DEFAULT_SYSTEM_PROMPT}{FINAL_ANSWER_STYLE}")
        );
    }

    #[test]
    fn build_prompt_ignores_the_graph_keys_that_are_not_this_executors() {
        let config = config_of(json!({"state_key": "llm", "reducer": "Replace"}));
        assert!(!config.tool_calling);
        assert!(config.available_tools.is_empty());
    }

    #[test]
    fn a_config_whose_tool_calling_is_not_a_bool_fails_the_node_at_the_entry() {
        assert!(matches!(
            LlmNodeConfig::parse(&json!({"tool_calling": "yes"})),
            Err(TaskError::Failed(_))
        ));
    }

    #[test]
    fn build_prompt_appends_tool_instructions_when_requested() {
        let (system, _) = build_prompt(&config_of(json!({"tool_calling": true})), &json!({}));
        assert!(system.contains("tool_call"));
    }

    #[test]
    fn build_prompt_renders_available_tools_into_the_system_prompt() {
        let config = config_of(
            json!({"available_tools": [{"name": "web_search", "description": "search the web"}]}),
        );
        let (system, _) = build_prompt(&config, &json!({}));
        assert!(system.contains("web_search"));
        assert!(system.contains("search the web"));
    }

    #[test]
    fn build_prompt_folds_prior_tool_results_into_the_user_prompt() {
        let state = json!({"question": "what is it?", "tool_result": ["62F, fog"]});
        let (_, user) = build_prompt(&config_of(json!({})), &state);
        assert!(user.contains("what is it?"));
        assert!(user.contains("62F, fog"));
    }

    #[test]
    fn build_prompt_renders_only_a_bounded_tail_of_the_tool_result_log() {
        let results: Vec<Value> = (0..RENDERED_TOOL_RESULTS + 3)
            .map(|index| json!(format!("result-{index}")))
            .collect();
        let last_index = results.len() - 1;
        let state = json!({"question": "q", "tool_result": results});

        let (_, user) = build_prompt(&config_of(json!({})), &state);

        assert!(!user.contains("result-0"), "{user}");
        assert!(user.contains(&format!("result-{last_index}")), "{user}");
        assert_eq!(
            user.matches("Tool result ").count(),
            RENDERED_TOOL_RESULTS,
            "{user}"
        );
        assert!(
            user.contains(&format!("Tool result {last_index}")),
            "an entry keeps its index in the full log: {user}"
        );
    }

    #[test]
    fn build_prompt_truncates_one_oversized_tool_result() {
        let state =
            json!({"question": "q", "tool_result": ["x".repeat(MAX_TOOL_RESULT_CHARS * 2)]});

        let (_, user) = build_prompt(&config_of(json!({})), &state);

        assert!(user.contains(TRUNCATION_MARK), "{user}");
        assert!(user.chars().count() < MAX_TOOL_RESULT_CHARS * 2, "{user}");
    }

    #[test]
    fn the_tool_use_policy_names_no_subject_and_no_tool() {
        let lowered = format!("{TOOL_USE_POLICY}{TOOL_NUDGE}{FINAL_ANSWER_STYLE}").to_lowercase();

        for subject_or_tool in [
            "weather",
            "news",
            "price",
            "score",
            "stock",
            "forecast",
            "web_search",
            "kb_search",
            "web_fetch",
            "knowledge base",
        ] {
            assert!(!lowered.contains(subject_or_tool), "{subject_or_tool}");
        }
    }

    #[test]
    fn build_prompt_requires_a_tool_before_answering_what_the_model_is_unsure_of() {
        let config = config_of(json!({
            "tool_calling": true,
            "available_tools": [{"name": "web_search", "description": "search the web"}],
        }));
        let (system, _) = build_prompt(&config, &json!({"question": "what is it today?"}));

        let tools_at = system.find("web_search").expect("catalog rendered");
        let policy_at = system.find("live and connected").expect("policy rendered");
        assert!(policy_at > tools_at, "{system}");
        assert!(
            system.contains("rather than answering from memory"),
            "{system}"
        );
        assert!(
            system.contains("Never reply that you lack access"),
            "{system}"
        );
    }

    #[test]
    fn build_prompt_demands_a_terse_spoken_answer() {
        let (system, _) = build_prompt(&config_of(json!({"tool_calling": true})), &json!({}));

        assert!(system.contains("read aloud"), "{system}");
        assert!(
            system.contains("a fragment is better than a sentence"),
            "{system}"
        );
        assert!(
            system.contains("Give the value, not a write-up"),
            "{system}"
        );
        assert!(system.contains("Never narrate your process"), "{system}");
        assert!(system.contains("Never add advice"), "{system}");
        let policy_at = system.find("live and connected").expect("policy rendered");
        let style_at = system.find("Final answer style").expect("style rendered");
        assert!(style_at > policy_at, "{system}");
    }

    #[test]
    fn build_prompt_requires_drilling_down_to_the_actual_value() {
        let (system, _) = build_prompt(&config_of(json!({"tool_calling": true})), &json!({}));

        assert!(
            system.contains("Keep going until you have the actual value"),
            "{system}"
        );
        assert!(
            system.contains("use what it gave you to call another"),
            "{system}"
        );
        assert!(system.contains("whichever responds first wins"), "{system}");
        assert!(
            system.contains("Never tell the user to go look somewhere else"),
            "{system}"
        );
        assert!(
            system.contains("never a description of what you are"),
            "{system}"
        );
        assert!(
            system.contains("Never decline because you are unsure a"),
            "{system}"
        );
    }

    #[test]
    fn build_prompt_without_tool_calling_carries_no_tool_use_policy() {
        let (system, _) = build_prompt(&config_of(json!({})), &json!({}));
        assert!(!system.contains("live and connected"), "{system}");
    }

    #[test]
    fn build_prompt_renders_a_tool_error_observation_so_the_model_can_recover() {
        let state = json!({
            "question": "what is it?",
            "tool_result": [{"error": "error sending request for url (https://example.invalid/)"}],
        });
        let (system, user) = build_prompt(&config_of(json!({"tool_calling": true})), &state);
        assert!(user.contains("ERROR"), "{user}");
        assert!(user.contains("example.invalid"), "{user}");
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
                &json!({"question": "what is it?"}),
                "e:llm:0",
            )
            .await
            .expect("execute");

        assert_eq!(output["tool_call"]["name"], json!("web_search"));
        let received = fake.received.lock().expect("lock");
        assert_eq!(received.len(), 1);
        assert_eq!(received[0].tier, EnumValue::Known(QualityTier::Medium));
        assert!(received[0].system_prompt.contains("tool_call"));
        assert_eq!(received[0].user_prompt, "what is it?");
        assert_eq!(
            received[0].sampling.response_format,
            Some(EnumValue::Known(ResponseFormat::JsonObject))
        );
    }

    #[test]
    fn the_nudge_condition_is_a_tool_less_first_answer_whatever_the_wording() {
        let tool_calling = LlmNodeConfig::parse(&json!({"tool_calling": true})).expect("config");
        let plain = LlmNodeConfig::parse(&json!({})).expect("config");
        let answered = json!({"tool_call": Value::Null, "reply": "anything at all"});
        let called_a_tool = json!({"tool_call": {"name": "some_tool"}, "reply": Value::Null});
        let no_tool_yet = json!({"question": "q"});
        let after_a_tool = json!({"question": "q", "tool_result": [{"ok": true}]});

        assert!(answered_before_consulting_any_tool(
            &tool_calling,
            &no_tool_yet,
            &answered
        ));
        assert!(
            !answered_before_consulting_any_tool(&tool_calling, &after_a_tool, &answered),
            "a reply that follows a real tool result is the answer the loop was for"
        );
        assert!(
            !answered_before_consulting_any_tool(&tool_calling, &no_tool_yet, &called_a_tool),
            "the model did use a tool"
        );
        assert!(
            !answered_before_consulting_any_tool(&plain, &no_tool_yet, &answered),
            "a node with no tools offered has nothing to be nudged towards"
        );
    }

    #[tokio::test]
    async fn an_answer_given_before_any_tool_ran_is_nudged_once_into_the_call() {
        let fake = Arc::new(FakeLlmRouter::then(
            r#"{"tool_call": null, "reply": "I do not have access to that."}"#,
            r#"{"tool_call": {"name": "some_search", "args": {"query": "q"}}}"#,
        ));
        let url = serve(Arc::clone(&fake)).await;
        let executor = LlmTaskExecutor::new(&url).expect("client");

        let output = executor
            .execute(
                "llm",
                &json!({"tool_calling": true}),
                &json!({"question": "which courses exist?"}),
                "e:llm:0",
            )
            .await
            .expect("execute");

        assert_eq!(output["tool_call"]["name"], json!("some_search"));
        assert_eq!(fake.calls(), 2);
        let received = fake.received.lock().expect("lock");
        assert!(received[1].system_prompt.contains("without using any tool"));
        assert!(received[1].system_prompt.contains("connected and working"));
        assert_eq!(output["usage"]["tokens_in"], json!(FAKE_TOKENS_IN * 2));
    }

    #[tokio::test]
    async fn an_answer_that_follows_a_tool_result_is_never_nudged() {
        let fake = Arc::new(FakeLlmRouter::always(
            r#"{"tool_call": null, "reply": "62F and foggy."}"#,
        ));
        let url = serve(Arc::clone(&fake)).await;
        let executor = LlmTaskExecutor::new(&url).expect("client");

        let output = executor
            .execute(
                "llm",
                &json!({"tool_calling": true}),
                &json!({"question": "q", "tool_result": ["62F, fog"]}),
                "e:llm:0",
            )
            .await
            .expect("execute");

        assert_eq!(output["reply"], json!("62F and foggy."));
        assert_eq!(fake.calls(), 1);
    }

    #[tokio::test]
    async fn a_node_without_tool_calling_is_never_nudged() {
        let fake = Arc::new(FakeLlmRouter::always(
            r#"{"tool_call": null, "reply": "Four."}"#,
        ));
        let url = serve(Arc::clone(&fake)).await;
        let executor = LlmTaskExecutor::new(&url).expect("client");

        executor
            .execute("llm", &json!({}), &json!({"question": "2+2?"}), "e:llm:0")
            .await
            .expect("execute");

        assert_eq!(fake.calls(), 1);
    }

    #[tokio::test]
    async fn a_model_that_answers_tool_lessly_twice_is_not_nudged_again() {
        let fake = Arc::new(FakeLlmRouter::always(
            r#"{"tool_call": null, "reply": "I will look into it."}"#,
        ));
        let url = serve(Arc::clone(&fake)).await;
        let executor = LlmTaskExecutor::new(&url).expect("client");

        executor
            .execute(
                "llm",
                &json!({"tool_calling": true}),
                &json!({"question": "q"}),
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
