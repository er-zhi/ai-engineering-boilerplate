// Runs an llm node by calling llm-router and parsing its reply.

use std::sync::LazyLock;
use std::time::Duration;

use buffa::EnumValue;
use common::execution_input::ExecutionInput;
use common::proto::llm_router::v1::{
    Answer, Choice, ChoiceAnswer, ChoiceOption, CompleteRequest, CompleteResponse, DecideRequest,
    DecideResponse, LlmRouterServiceClient, Noul, QualityTier, Question, ResponseFormat, Sampling,
    SystemOneServiceClient, answer::Answer as Given,
};
use common::proto::tools::v1::{ExecuteRequest, ExecuteResponse, ExecuteStatus, ToolServiceClient};
use connectrpc::client::HttpClient;
use engine_core::{
    FAST_TOOL_CALL_FIELD, LlmOutput, TOOL_RESULT_ERROR_KEY, TOOL_RESULT_STATE_KEY, TaskError,
    TaskExecutor, ToolCall,
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

/// `Decide` gets its own deadline, separate from `CALL_TIMEOUT` (`Complete`'s). Same order and
/// reasoning as `services/chat/src/intent.rs`'s `DECIDE_CALL_TIMEOUT` and
/// `services/tool/src/service.rs`'s `VALIDATE_TIMEOUT`: the vendor typically answers in about
/// 100 ms, so 2 s covers a cold connection, a retry the vendor's SDK performs before it reports
/// back, and this service's own hop out to llm-router and back — not a generous budget for the
/// vendor, a bound on how long this node waits before falling back to today's flow.
///
/// **Must stay strictly above `services/llm-router`'s `adapters::system_one::REQUEST_TIMEOUT`
/// (1.5 s), for the same reason Chat's and Tool's matching constants must**: this deadline is
/// asserted as the `grpc-timeout` header llm-router's connectrpc server parses on receipt, wrapping
/// the *entire* dispatch to the vendor in a deadline that starts strictly before the adapter's own
/// clock does. Equal or shorter and this outer deadline always wins that race, silently dropping
/// llm-router's decision future — audit write included — before the inner timeout ever finishes.
const DECIDE_CALL_TIMEOUT: Duration = Duration::from_secs(2);
/// A wrong or slow tool dispatched on the fast path is exactly one tool call, so it gets the same
/// budget the normal `tool` node gives one — see `executors::tool::CALL_TIMEOUT`, which this
/// mirrors (that constant is private to its own module, so this is its own copy, not a shared one).
const TOOL_CALL_TIMEOUT: Duration = Duration::from_secs(60);

const NEEDS_EXTERNAL_INFO_ID: &str = "needs_external_information";
const TOOL_QUESTION_ID: &str = "tool";
const NO_TOOL_OPTION: &str = "none";

/// Phrased behaviourally, naming no subject: the repo's `gate-architecture` rule "Capabilities, Not
/// Topics" forbids a domain or topic anywhere in code or prompts, and this text is read by the
/// typed decider, not the model, so it never gets a tool's own description mixed into it.
const NEEDS_EXTERNAL_INFO_INSTRUCTIONS: &str = "Does answering this question require information \
not present here — something current, something specific, or something about this system's own \
data?";
const NEEDS_EXTERNAL_INFO_WHEN_TRUE: &str = "the answer depends on something current, something \
specific that would have to be looked up, or something about this system's own data";
const NEEDS_EXTERNAL_INFO_WHEN_FALSE: &str = "the question can be answered from general knowledge or reasoning alone, with nothing \
current, specific, or system-specific to look up";
const TOOL_QUESTION_INSTRUCTIONS: &str = "Which of the listed tools, if any, should be called to answer this question directly? \
Choose the option named for no-tool if none of them is needed.";

/// Below this the model cannot separate "no information needed" from real doubt, and skipping the
/// nudge on a wrong guess costs the turn's whole answer, not one extra call — so this asks for real
/// confidence, not just a lean, before the nudge is given up.
const NEEDS_EXTERNAL_INFO_CONFIDENT_FALSE_THRESHOLD: f64 = 0.3;
/// A wrong tool pick on this path costs one tool call, not the turn — the normal loop continues
/// after it — so this does not need the same certainty as skipping the nudge above.
const TOOL_CHOICE_CONFIDENCE_THRESHOLD: f64 = 0.5;
/// The question is sent to a fast-dispatched tool verbatim, as the whole value of its one string
/// argument — never composed by a model first. That is only safe for something shaped like a
/// question: `services/chat/src/topic_turn.rs` builds a follow-up's question as `"Earlier
/// answer:\n{summary}\n\nFollow-up: {content}"`, multi-line and capable of embedding a previous
/// answer's whole text. Sending that straight into a tool's query would both wreck the tool's own
/// result quality and ship a prior answer into a third-party provider's request. A single line
/// under this length is what "verbatim" was written for; anything longer or multi-line falls
/// through to today's flow, where the model composes its own query instead.
const MAX_FAST_DISPATCH_QUESTION_CHARS: usize = 200;

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
    /// The tool's `input_schema_json`, exactly as `ListTools` published it — read at runtime by
    /// `single_required_string_field` to decide whether a confident pick may be dispatched without
    /// a generative turn first. Never inspected for a tool's name: the gate this module's tests
    /// enforce (`the_tool_use_policy_names_no_subject_and_no_tool`) is about naming a subject in
    /// code or prompts, not about reading a live catalog's own schema field.
    pub input_schema_json: String,
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
    decider: SystemOneServiceClient<HttpClient>,
    tool: ToolServiceClient<HttpClient>,
}

impl LlmTaskExecutor {
    pub fn new(llm_router_url: &str, tool_service_url: &str) -> Result<Self, String> {
        Ok(Self {
            client: crate::executors::grpc_client(
                "LLM_ROUTER_URL",
                llm_router_url,
                CALL_TIMEOUT,
                LlmRouterServiceClient::new,
            )?,
            decider: crate::executors::grpc_client(
                "LLM_ROUTER_URL",
                llm_router_url,
                DECIDE_CALL_TIMEOUT,
                SystemOneServiceClient::new,
            )?,
            tool: crate::executors::grpc_client(
                "TOOL_SERVICE_URL",
                tool_service_url,
                TOOL_CALL_TIMEOUT,
                ToolServiceClient::new,
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

        if let Some(fast_result) = self.try_fast_path(&config, state, idempotency_key).await {
            return fast_result;
        }

        let request = complete_request(&config, state);
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

fn complete_request(config: &LlmNodeConfig, state: &Value) -> CompleteRequest {
    let (system_prompt, user_prompt) = build_prompt(config, state);
    let response_format = config
        .tool_calling
        .then_some(EnumValue::Known(ResponseFormat::JsonObject));
    CompleteRequest {
        tier: EnumValue::Known(QualityTier::Medium),
        system_prompt,
        user_prompt,
        sampling: Sampling {
            response_format,
            ..Default::default()
        }
        .into(),
        ..Default::default()
    }
}

/// What one typed `Decide` call, sent before the first generative call of a turn, resolves to.
/// `decide_usage` is `Decide`'s own token cost, shaped for `merge_usage` — see
/// `decide_usage_value` — so `try_fast_path` can fold it into the budget the same way
/// `merge_usage` already folds the nudge's second `Complete` in.
enum FastPath {
    /// `needs_external_information` came back confidently false, and `tool` named nothing
    /// confidently: answer with one `Complete`, nudge disabled.
    NoToolNeeded { decide_usage: Value },
    /// `tool` came back confident, named a live catalog entry, that entry's schema takes exactly
    /// one required string field, and the question is shaped like something safe to send
    /// verbatim: call it with the question verbatim.
    ToolCall { call: ToolCall, decide_usage: Value },
    /// No typed decision was reached (an error or a timeout), or the decision reached is neither
    /// of the above — including a confident tool pick that failed the schema or question-shape
    /// gate, which vetoes `NoToolNeeded` too: the decider has said a tool is needed, and that
    /// outranks a separate, independently-evaluated "no information needed". Today's flow,
    /// unchanged, nudge included.
    Unavailable,
}

impl LlmTaskExecutor {
    /// `Some` when the fast path decided the whole node's outcome — either answer directly.
    /// `None` means "fall through to today's flow", the fail-safe for every unavailable or
    /// unconfident `Decide` outcome.
    ///
    /// Only tried on the very first generative call of a turn — `config.tool_calling` and no
    /// `tool_result` yet in state — matching `answered_before_consulting_any_tool`'s own gate: a
    /// later iteration already has tool results the typed decision was never asked about, so it
    /// takes today's flow instead.
    async fn try_fast_path(
        &self,
        config: &LlmNodeConfig,
        state: &Value,
        idempotency_key: &str,
    ) -> Option<Result<Value, TaskError>> {
        if !config.tool_calling || !no_tool_results_yet(state) {
            return None;
        }
        let question = ExecutionInput::in_state(state).question;
        match self.decide_fast_path(config, &question).await {
            FastPath::ToolCall { call, decide_usage } => Some(
                self.run_fast_tool_call(config, state, &call, &decide_usage, idempotency_key)
                    .await,
            ),
            FastPath::NoToolNeeded { decide_usage } => {
                let request = complete_request(config, state);
                let outcome = self
                    .complete_with_retries(&request)
                    .await
                    .map(|output| merge_usage(&decide_usage, output));
                Some(outcome)
            }
            FastPath::Unavailable => None,
        }
    }

    /// Dispatches the fast-chosen tool, folds its result into the prompt for one `Complete` call,
    /// merges in both spent calls' token usage, and records the call on the node's own output
    /// under `FAST_TOOL_CALL_FIELD` — see that constant's doc for why that alone makes the call
    /// event-logged, checkpointed and budget-charged by the ordinary per-node machinery.
    async fn run_fast_tool_call(
        &self,
        config: &LlmNodeConfig,
        state: &Value,
        call: &ToolCall,
        decide_usage: &Value,
        idempotency_key: &str,
    ) -> Result<Value, TaskError> {
        let result = self.dispatch_tool_fast(call, idempotency_key).await;
        let state_with_result = state_with_extra_tool_result(state, result.clone());
        let request = complete_request(config, &state_with_result);
        let output = self.complete_with_retries(&request).await?;
        let output = merge_usage(decide_usage, output);
        Ok(attach_fast_tool_call(output, call, result))
    }

    /// Sends one `Decide` whose state is the question alone — not the whole execution state, per
    /// the vendor's own guidance. Any error or timeout is `FastPath::Unavailable`, the fail-safe
    /// that keeps a `Decide` outage from ever costing a turn. The vendor evaluates each question
    /// independently (`llm_router.proto`'s `DecideRequest` doc), so a confident `tool` pick and a
    /// confident "no information needed" can — and do — co-occur; `confident_tool_choice` is
    /// checked first for exactly that reason, and a confident pick that fails the fast-dispatch
    /// gate still returns `Unavailable`, never falling through to `NoToolNeeded`.
    async fn decide_fast_path(&self, config: &LlmNodeConfig, question: &str) -> FastPath {
        let Some(request) = decide_request(question, &config.available_tools) else {
            return FastPath::Unavailable;
        };
        let decided = match self.decider.decide(request).await {
            Ok(response) => response.into_owned(),
            Err(error) => {
                tracing::warn!(
                    %error,
                    "typed decision failed, falling back to today's flow with the nudge enabled"
                );
                return FastPath::Unavailable;
            }
        };
        let decide_usage = decide_usage_value(&decided);

        if let Some(choice) = confident_tool_choice(&decided.answers) {
            return match fast_dispatchable_tool_call(choice, &config.available_tools, question) {
                Some(call) => FastPath::ToolCall { call, decide_usage },
                None => FastPath::Unavailable,
            };
        }
        if find_noul(&decided.answers, NEEDS_EXTERNAL_INFO_ID)
            .is_some_and(|value| value < NEEDS_EXTERNAL_INFO_CONFIDENT_FALSE_THRESHOLD)
        {
            return FastPath::NoToolNeeded { decide_usage };
        }
        FastPath::Unavailable
    }

    /// Runs the fast-dispatched tool the same way a failed normal tool call would be shown to the
    /// model: as an observation, either the raw output or `{"error": ...}` — never a hard failure.
    /// A wrong pick on this path costs one tool call, not the turn, so nothing here can fail the
    /// node; the answering `Complete` that follows sees whatever this returns and can recover from
    /// it exactly as it recovers from a normal tool node's error.
    async fn dispatch_tool_fast(&self, call: &ToolCall, idempotency_key: &str) -> Value {
        let request = ExecuteRequest {
            slug: call.name.clone(),
            input_json: call.args.to_string(),
            idempotency_key: format!("{idempotency_key}:decide-tool"),
            ..Default::default()
        };
        match self.tool.execute(request).await {
            Ok(response) => tool_result_value(&response.into_owned()),
            Err(error) => json!({ TOOL_RESULT_ERROR_KEY: error.to_string() }),
        }
    }
}

/// `Decide`'s own token cost, shaped like a node output's `usage` object so `merge_usage` — built
/// to fold the nudge's second `Complete` into the first — folds this in the same way. Every
/// fast-path turn spends this call and the budget must see it, same as any other.
fn decide_usage_value(decided: &DecideResponse) -> Value {
    json!({"usage": {"tokens_in": decided.tokens_in, "tokens_out": decided.tokens_out}})
}

/// Records the fast-dispatched call on the node's own output — name, args and result, under
/// `FAST_TOOL_CALL_FIELD` — which `engine-core::step::apply_output` then records verbatim in this
/// node's `NodeCompleted` event and writes into the checkpointed state through the node's own
/// reducer, and `engine-core::step::charge_budget` reads to charge the extra `tool_calls_remaining`
/// unit a `tool` node would otherwise have earned.
fn attach_fast_tool_call(mut output: Value, call: &ToolCall, result: Value) -> Value {
    if let Some(object) = output.as_object_mut() {
        object.insert(
            FAST_TOOL_CALL_FIELD.to_owned(),
            json!({"name": call.name, "args": call.args, "result": result}),
        );
    }
    output
}

fn tool_result_value(response: &ExecuteResponse) -> Value {
    if response.status == EnumValue::Known(ExecuteStatus::Ok) {
        return serde_json::from_str(&response.output_json)
            .unwrap_or_else(|_| json!({ TOOL_RESULT_ERROR_KEY: "tool returned invalid JSON" }));
    }
    let message = if response.error_message.is_empty() {
        "tool call failed".to_owned()
    } else {
        response.error_message.clone()
    };
    json!({ TOOL_RESULT_ERROR_KEY: message })
}

fn no_tool_results_yet(state: &Value) -> bool {
    state
        .get(TOOL_RESULT_STATE_KEY)
        .and_then(Value::as_array)
        .is_none_or(Vec::is_empty)
}

fn state_with_extra_tool_result(state: &Value, result: Value) -> Value {
    let mut state = state.clone();
    let mut results: Vec<Value> = state
        .get(TOOL_RESULT_STATE_KEY)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    results.push(result);
    if let Some(object) = state.as_object_mut() {
        object.insert(TOOL_RESULT_STATE_KEY.to_owned(), Value::Array(results));
    }
    state
}

/// The JSON `Decide` reads: the question alone, not the whole execution state — see
/// `try_fast_path`'s doc for why. Two questions ride the same call: `needs_external_information`
/// is read only when no confident, schema-eligible tool pick is found — see `decide_fast_path`.
fn decide_request(question: &str, available_tools: &[CatalogEntry]) -> Option<DecideRequest> {
    let mut request: DecideRequest = serde_json::from_value(json!({ "state": question })).ok()?;
    request.questions = vec![
        needs_external_information_question()?,
        tool_question(available_tools)?,
    ];
    Some(request)
}

fn instructed(id: &str, instructions: Value) -> Option<Question> {
    let mut question: Question =
        serde_json::from_value(json!({ "instructions": instructions })).ok()?;
    question.id = id.to_owned();
    Some(question)
}

fn needs_external_information_question() -> Option<Question> {
    let mut question = instructed(
        NEEDS_EXTERNAL_INFO_ID,
        json!(NEEDS_EXTERNAL_INFO_INSTRUCTIONS),
    )?;
    question.kind = Noul {
        when_true: Some(NEEDS_EXTERNAL_INFO_WHEN_TRUE.to_owned()),
        when_false: Some(NEEDS_EXTERNAL_INFO_WHEN_FALSE.to_owned()),
        ..Default::default()
    }
    .into();
    Some(question)
}

/// One option per entry in the live catalog the dispatcher injected, plus `NO_TOOL_OPTION` — read
/// from `available_tools` at runtime, never a name written in Rust.
fn tool_question(available_tools: &[CatalogEntry]) -> Option<Question> {
    let mut options: Vec<ChoiceOption> = available_tools
        .iter()
        .map(|tool| ChoiceOption {
            name: tool.name.clone(),
            description: (!tool.description.is_empty()).then(|| tool.description.clone()),
            ..Default::default()
        })
        .collect();
    options.push(ChoiceOption {
        name: NO_TOOL_OPTION.to_owned(),
        description: None,
        ..Default::default()
    });
    let mut question = instructed(TOOL_QUESTION_ID, json!(TOOL_QUESTION_INSTRUCTIONS))?;
    question.kind = Choice {
        options,
        ..Default::default()
    }
    .into();
    Some(question)
}

/// The `tool` choice, if it names something other than `NO_TOOL_OPTION` at or above the
/// confidence threshold — regardless of whether it goes on to qualify for fast dispatch. A
/// confident pick here means the decider has judged a tool necessary, and callers must treat that
/// as vetoing `FastPath::NoToolNeeded` even when the pick itself cannot be fast-dispatched — see
/// `decide_fast_path`.
fn confident_tool_choice(answers: &[Answer]) -> Option<&ChoiceAnswer> {
    find_choice(answers, TOOL_QUESTION_ID).filter(|choice| {
        choice.choice != NO_TOOL_OPTION && choice.confidence >= TOOL_CHOICE_CONFIDENCE_THRESHOLD
    })
}

/// A confident `tool` pick additionally qualifies for the fast path only when it names a live
/// catalog entry whose `input_schema` takes exactly one required string field — decided by the
/// schema, never by the tool's name — and `question` is shaped like something safe to send
/// verbatim (see `MAX_FAST_DISPATCH_QUESTION_CHARS`'s doc). `question` becomes that field's value.
fn fast_dispatchable_tool_call(
    choice: &ChoiceAnswer,
    available_tools: &[CatalogEntry],
    question: &str,
) -> Option<ToolCall> {
    if !fits_fast_dispatch(question) {
        return None;
    }
    let tool = available_tools
        .iter()
        .find(|tool| tool.name == choice.choice)?;
    let field = single_required_string_field(&tool.input_schema_json)?;
    Some(ToolCall {
        name: tool.name.clone(),
        args: json!({ field: question }),
    })
}

/// Whether `question` is short enough and shaped enough (one line) to hand a fast-dispatched tool
/// verbatim as its whole one-string argument. See `MAX_FAST_DISPATCH_QUESTION_CHARS`'s doc for why
/// this exists: a follow-up's question is not always a question.
fn fits_fast_dispatch(question: &str) -> bool {
    let trimmed = question.trim();
    !trimmed.is_empty()
        && !trimmed.contains('\n')
        && trimmed.chars().count() <= MAX_FAST_DISPATCH_QUESTION_CHARS
}

/// `Some(field)` when `input_schema_json` is a JSON Schema object whose `required` names exactly
/// one field, and that field's own schema is `"type": "string"`. A tool needing structured
/// arguments — two required fields, a non-string field, or a schema this cannot parse — is `None`,
/// so it never takes the fast path regardless of how confidently it was chosen.
fn single_required_string_field(input_schema_json: &str) -> Option<String> {
    let schema: Value = serde_json::from_str(input_schema_json).ok()?;
    let required = schema.get("required")?.as_array()?;
    let [only] = required.as_slice() else {
        return None;
    };
    let field = only.as_str()?;
    let is_string = schema
        .get("properties")
        .and_then(|properties| properties.get(field))
        .and_then(|field_schema| field_schema.get("type"))
        .and_then(Value::as_str)
        == Some("string");
    is_string.then(|| field.to_owned())
}

fn find_noul(answers: &[Answer], id: &str) -> Option<f64> {
    answers
        .iter()
        .find(|answer| answer.id == id)
        .and_then(|answer| match answer.answer.as_ref() {
            Some(Given::Noul(noul)) => Some(noul.noul),
            _ => None,
        })
}

fn find_choice<'a>(answers: &'a [Answer], id: &str) -> Option<&'a ChoiceAnswer> {
    answers
        .iter()
        .find(|answer| answer.id == id)
        .and_then(|answer| match answer.answer.as_ref() {
            Some(Given::Choice(choice)) => Some(choice.as_ref()),
            _ => None,
        })
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
    config.tool_calling && written.tool_call.is_null() && no_tool_results_yet(state)
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
        let lowered = format!(
            "{TOOL_USE_POLICY}{TOOL_NUDGE}{FINAL_ANSWER_STYLE}{NEEDS_EXTERNAL_INFO_INSTRUCTIONS}\
             {NEEDS_EXTERNAL_INFO_WHEN_TRUE}{NEEDS_EXTERNAL_INFO_WHEN_FALSE}{TOOL_QUESTION_INSTRUCTIONS}"
        )
        .to_lowercase();

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
        DecideResponse, DescribeModelsRequest, DescribeModelsResponse, DescribeTiersRequest,
        DescribeTiersResponse, LlmRouterService, LlmRouterServiceRegisterMarker, NoulAnswer,
        SystemOneService, SystemOneServiceRegisterMarker,
    };
    use common::proto::tools::v1::{
        ActivateToolRequest, ActivateToolResponse, CreateToolRequest, CreateToolResponse,
        ListToolsRequest, ListToolsResponse, ToolService, ValidateToolRequest,
        ValidateToolResponse,
    };
    use connectrpc::{
        RequestContext, Response, Router as ConnectRouter, ServiceRequest, ServiceResult,
    };

    const FAKE_TOKENS_IN: i32 = 17;
    const FAKE_TOKENS_OUT: i32 = 23;
    const FAKE_DECIDE_TOKENS_IN: i32 = 5;
    const FAKE_DECIDE_TOKENS_OUT: i32 = 2;
    /// Stands in for a tool service this test never expects to be called — any decide/dispatch
    /// path that did reach it would fail loudly, since nothing is listening here.
    const UNREACHABLE: &str = "http://127.0.0.1:1";

    struct FakeLlmRouter {
        received: Mutex<Vec<CompleteRequest>>,
        reply: String,
        then_reply: Option<String>,
        decide_tool: Mutex<Option<(String, f64)>>,
        decide_needs_external_info: Mutex<Option<f64>>,
        decide_calls: Mutex<usize>,
        decide_hangs: Mutex<Option<Arc<tokio::sync::Barrier>>>,
    }

    impl FakeLlmRouter {
        fn always(reply: &str) -> Self {
            Self {
                received: Mutex::new(Vec::new()),
                reply: reply.to_owned(),
                then_reply: None,
                decide_tool: Mutex::new(None),
                decide_needs_external_info: Mutex::new(None),
                decide_calls: Mutex::new(0),
                decide_hangs: Mutex::new(None),
            }
        }

        fn then(reply: &str, then_reply: &str) -> Self {
            Self {
                received: Mutex::new(Vec::new()),
                reply: reply.to_owned(),
                then_reply: Some(then_reply.to_owned()),
                decide_tool: Mutex::new(None),
                decide_needs_external_info: Mutex::new(None),
                decide_calls: Mutex::new(0),
                decide_hangs: Mutex::new(None),
            }
        }

        fn calls(&self) -> usize {
            self.received.lock().expect("lock").len()
        }

        fn decide_calls(&self) -> usize {
            *self.decide_calls.lock().expect("lock")
        }

        /// Arms `Decide`'s `tool` answer. Leaving both this and
        /// `answer_decide_needs_external_info` unset makes `decide` fail, matching an unarmed
        /// `Complete` — the fail-safe every non-scripted existing test in this module exercises.
        fn answer_decide_tool(&self, choice: &str, confidence: f64) {
            *self.decide_tool.lock().expect("lock") = Some((choice.to_owned(), confidence));
        }

        fn answer_decide_needs_external_info(&self, noul: f64) {
            *self.decide_needs_external_info.lock().expect("lock") = Some(noul);
        }

        /// Accepts a `Decide` call and then never answers it, so a test can observe it abandoned
        /// at `DECIDE_CALL_TIMEOUT` rather than any particular answer.
        fn hang_decide(&self, barrier: Arc<tokio::sync::Barrier>) {
            *self.decide_hangs.lock().expect("lock") = Some(barrier);
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

    #[allow(refining_impl_trait)]
    impl SystemOneService for FakeLlmRouter {
        async fn decide(
            &self,
            _ctx: RequestContext,
            request: ServiceRequest<'_, DecideRequest>,
        ) -> ServiceResult<DecideResponse> {
            *self.decide_calls.lock().expect("lock") += 1;
            let barrier = self.decide_hangs.lock().expect("lock").clone();
            if let Some(barrier) = barrier {
                barrier.wait().await;
            }
            let tool = self.decide_tool.lock().expect("lock").clone();
            let needs_info = *self.decide_needs_external_info.lock().expect("lock");
            if tool.is_none() && needs_info.is_none() {
                return Err(connectrpc::ConnectError::unavailable(
                    "configured fake decider failure",
                ));
            }
            let owned = request.to_owned_message();
            let answers = owned
                .questions
                .iter()
                .filter_map(|question| scripted_answer(&question.id, &tool, needs_info))
                .collect();
            Response::ok(DecideResponse {
                answers,
                tokens_in: FAKE_DECIDE_TOKENS_IN,
                tokens_out: FAKE_DECIDE_TOKENS_OUT,
                ..Default::default()
            })
        }

        async fn describe_models(
            &self,
            _ctx: RequestContext,
            _request: ServiceRequest<'_, DescribeModelsRequest>,
        ) -> ServiceResult<DescribeModelsResponse> {
            Response::ok(DescribeModelsResponse::default())
        }
    }

    fn scripted_answer(
        id: &str,
        tool: &Option<(String, f64)>,
        needs_info: Option<f64>,
    ) -> Option<Answer> {
        match id {
            TOOL_QUESTION_ID => tool.as_ref().map(|(choice, confidence)| Answer {
                id: TOOL_QUESTION_ID.to_owned(),
                answer: ChoiceAnswer {
                    choice: choice.clone(),
                    confidence: *confidence,
                    ..Default::default()
                }
                .into(),
                ..Default::default()
            }),
            NEEDS_EXTERNAL_INFO_ID => needs_info.map(|noul| Answer {
                id: NEEDS_EXTERNAL_INFO_ID.to_owned(),
                answer: NoulAnswer {
                    noul,
                    ..Default::default()
                }
                .into(),
                ..Default::default()
            }),
            _ => None,
        }
    }

    // `FakeLlmRouter` implements both traits on one type, so `add_service` needs the register
    // marker for each — see `services/chat/src/fakes.rs::router_service` for the same idiom.
    fn router_service(fake: Arc<FakeLlmRouter>) -> ConnectRouter {
        ConnectRouter::new()
            .add_service::<_, LlmRouterServiceRegisterMarker>(Arc::clone(&fake))
            .add_service::<_, SystemOneServiceRegisterMarker>(fake)
    }

    async fn serve(fake: Arc<FakeLlmRouter>) -> String {
        let app = axum::Router::new().fallback_service(router_service(fake).into_axum_service());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("addr");
        tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
        format!("http://{address}")
    }

    struct FakeToolService {
        received: Mutex<Vec<ExecuteRequest>>,
        output_json: String,
    }

    impl FakeToolService {
        fn ok(output_json: &str) -> Self {
            Self {
                received: Mutex::new(Vec::new()),
                output_json: output_json.to_owned(),
            }
        }

        fn calls(&self) -> usize {
            self.received.lock().expect("lock").len()
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
                status: EnumValue::Known(ExecuteStatus::Ok),
                output_json: self.output_json.clone(),
                ..Default::default()
            })
        }
        async fn list_tools(
            &self,
            _ctx: RequestContext,
            _request: ServiceRequest<'_, ListToolsRequest>,
        ) -> ServiceResult<ListToolsResponse> {
            Response::ok(ListToolsResponse::default())
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

    async fn serve_tool(fake: Arc<FakeToolService>) -> String {
        let connect = ConnectRouter::new().add_service(fake);
        let app = axum::Router::new().fallback_service(connect.into_axum_service());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("addr");
        tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
        format!("http://{address}")
    }

    fn single_field_tool(name: &str, field: &str) -> CatalogEntry {
        CatalogEntry {
            name: name.to_owned(),
            description: "does a thing".to_owned(),
            input_schema_json: json!({
                "type": "object",
                "required": [field],
                "properties": {field: {"type": "string"}},
            })
            .to_string(),
        }
    }

    fn two_field_tool(name: &str) -> CatalogEntry {
        CatalogEntry {
            name: name.to_owned(),
            description: "does a thing".to_owned(),
            input_schema_json: json!({
                "type": "object",
                "required": ["a", "b"],
                "properties": {"a": {"type": "string"}, "b": {"type": "string"}},
            })
            .to_string(),
        }
    }

    #[tokio::test]
    async fn execute_sends_the_built_prompt_and_parses_a_tool_call_reply() {
        let fake = Arc::new(FakeLlmRouter::always(
            r#"{"tool_call": {"name": "web_search", "args": {"query": "sf weather"}}}"#,
        ));
        let url = serve(Arc::clone(&fake)).await;
        let executor = LlmTaskExecutor::new(&url, UNREACHABLE).expect("client");

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
        let executor = LlmTaskExecutor::new(&url, UNREACHABLE).expect("client");

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
        let executor = LlmTaskExecutor::new(&url, UNREACHABLE).expect("client");

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
        let executor = LlmTaskExecutor::new(&url, UNREACHABLE).expect("client");

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
        let executor = LlmTaskExecutor::new(&url, UNREACHABLE).expect("client");

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
        let executor = LlmTaskExecutor::new(&url, UNREACHABLE).expect("client");

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
        let executor = LlmTaskExecutor::new(&url, UNREACHABLE).expect("client");

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

    #[test]
    fn single_required_string_field_accepts_exactly_one_required_string() {
        let schema = json!({
            "type": "object",
            "required": ["query"],
            "properties": {"query": {"type": "string"}},
        })
        .to_string();
        assert_eq!(
            single_required_string_field(&schema),
            Some("query".to_owned())
        );
    }

    // The shape every tool that actually qualifies in production has: one required string field
    // plus an optional sibling — e.g. web_search / kb_search's real `{"required": ["query"],
    // "properties": {"query": {...}, "limit": {...}}}`. A bare one-field schema with no sibling,
    // as the other tests here use, is not what a live catalog entry looks like.
    #[test]
    fn single_required_string_field_accepts_a_required_string_alongside_an_optional_sibling() {
        let schema = json!({
            "type": "object",
            "required": ["query"],
            "properties": {
                "query": {"type": "string"},
                "limit": {"type": ["integer", "null"]},
            },
        })
        .to_string();
        assert_eq!(
            single_required_string_field(&schema),
            Some("query".to_owned())
        );
    }

    #[test]
    fn single_required_string_field_rejects_two_required_fields() {
        let schema = json!({
            "type": "object",
            "required": ["a", "b"],
            "properties": {"a": {"type": "string"}, "b": {"type": "string"}},
        })
        .to_string();
        assert_eq!(single_required_string_field(&schema), None);
    }

    #[test]
    fn single_required_string_field_rejects_a_non_string_required_field() {
        let schema = json!({
            "type": "object",
            "required": ["count"],
            "properties": {"count": {"type": "number"}},
        })
        .to_string();
        assert_eq!(single_required_string_field(&schema), None);
    }

    #[test]
    fn single_required_string_field_rejects_a_schema_it_cannot_read() {
        assert_eq!(single_required_string_field("not json"), None);
        assert_eq!(single_required_string_field("{}"), None);
        assert_eq!(single_required_string_field(r#"{"required": []}"#), None);
    }

    #[tokio::test]
    async fn a_confident_no_external_information_needed_answer_skips_the_nudge() {
        let fake = Arc::new(FakeLlmRouter::always(
            r#"{"tool_call": null, "reply": "68"}"#,
        ));
        fake.answer_decide_needs_external_info(0.02);
        let url = serve(Arc::clone(&fake)).await;
        let executor = LlmTaskExecutor::new(&url, UNREACHABLE).expect("client");

        let output = executor
            .execute(
                "llm",
                &json!({"tool_calling": true}),
                &json!({"question": "what is 17 times 4?"}),
                "e:llm:0",
            )
            .await
            .expect("execute");

        assert_eq!(output["reply"], json!("68"));
        assert_eq!(
            fake.decide_calls(),
            1,
            "one Decide call precedes the answer"
        );
        assert_eq!(
            fake.calls(),
            1,
            "a confident no-external-information-needed answer must cost exactly one Complete \
             call, with the nudge disabled — the reply above carries no tool_call, so a nudge \
             would otherwise have fired"
        );
        assert_eq!(
            output["usage"]["tokens_in"],
            json!(i64::from(FAKE_TOKENS_IN + FAKE_DECIDE_TOKENS_IN)),
            "the Decide call this path spends must be charged against the budget too, not just \
             the Complete call — the nudge path already does this with merge_usage"
        );
        assert_eq!(
            output["usage"]["tokens_out"],
            json!(i64::from(FAKE_TOKENS_OUT + FAKE_DECIDE_TOKENS_OUT))
        );
    }

    async fn dispatched_fast_tool_call() -> (Value, Arc<FakeLlmRouter>, Arc<FakeToolService>) {
        let fake = Arc::new(FakeLlmRouter::always(
            r#"{"tool_call": null, "reply": "sixty-eight"}"#,
        ));
        fake.answer_decide_tool("some_tool", 0.9);
        let llm_url = serve(Arc::clone(&fake)).await;
        let tool = Arc::new(FakeToolService::ok(r#"{"answer": "sixty-eight"}"#));
        let tool_url = serve_tool(Arc::clone(&tool)).await;
        let executor = LlmTaskExecutor::new(&llm_url, &tool_url).expect("client");
        let config = LlmNodeConfig {
            tool_calling: true,
            available_tools: vec![single_field_tool("some_tool", "query")],
            ..Default::default()
        }
        .to_json();

        let output = executor
            .execute(
                "llm",
                &config,
                &json!({"question": "what is 17 times 4?"}),
                "e:llm:0",
            )
            .await
            .expect("execute");
        (output, fake, tool)
    }

    #[tokio::test]
    async fn a_confident_single_string_field_tool_is_dispatched_without_a_generative_turn_first() {
        let (output, fake, tool) = dispatched_fast_tool_call().await;

        assert_eq!(output["reply"], json!("sixty-eight"));
        assert_eq!(
            tool.calls(),
            1,
            "the confidently chosen single-string-field tool must be dispatched directly"
        );
        let tool_calls = tool.received.lock().expect("lock");
        assert_eq!(tool_calls[0].slug, "some_tool");
        assert_eq!(
            tool_calls[0].input_json, r#"{"query":"what is 17 times 4?"}"#,
            "the question is passed verbatim as the tool's one required field"
        );
        drop(tool_calls);
        assert_eq!(
            fake.calls(),
            1,
            "the tool was dispatched without a generative turn deciding it first — one Complete \
             call total"
        );
        let received = fake.received.lock().expect("lock");
        assert!(
            received[0].user_prompt.contains("sixty-eight"),
            "the dispatched tool's result must be appended before the answering Complete: {}",
            received[0].user_prompt
        );
    }

    // The evidence Critical finding 1 asked for: the fast-dispatched call must be durable, not
    // just used to answer and then dropped.
    #[tokio::test]
    async fn a_fast_dispatched_tool_call_is_recorded_on_the_nodes_own_output() {
        let (output, ..) = dispatched_fast_tool_call().await;

        assert_eq!(
            output["fast_tool_call"],
            json!({
                "name": "some_tool",
                "args": {"query": "what is 17 times 4?"},
                "result": {"answer": "sixty-eight"},
            }),
            "the fast-dispatched call must be recorded on the node's own output, so it is \
             event-logged and checkpointed by the ordinary per-node machinery — otherwise the \
             most common tool call in the system is invisible"
        );
        assert_eq!(
            output["usage"]["tokens_in"],
            json!(i64::from(FAKE_TOKENS_IN + FAKE_DECIDE_TOKENS_IN)),
            "the Decide call this path spends must be charged too, the same as the nudge's \
             second Complete call already is via merge_usage"
        );
    }

    #[tokio::test]
    async fn a_tool_needing_two_fields_is_not_taken_on_the_fast_path_even_when_confidently_chosen()
    {
        let fake = Arc::new(FakeLlmRouter::always(
            r#"{"tool_call": {"name": "two_field_tool", "args": {"a": "x", "b": "y"}}}"#,
        ));
        fake.answer_decide_tool("two_field_tool", 0.95);
        let llm_url = serve(Arc::clone(&fake)).await;
        let tool = Arc::new(FakeToolService::ok("{}"));
        let tool_url = serve_tool(Arc::clone(&tool)).await;
        let executor = LlmTaskExecutor::new(&llm_url, &tool_url).expect("client");
        let config = LlmNodeConfig {
            tool_calling: true,
            available_tools: vec![two_field_tool("two_field_tool")],
            ..Default::default()
        }
        .to_json();

        executor
            .execute("llm", &config, &json!({"question": "q"}), "e:llm:0")
            .await
            .expect("execute");

        assert_eq!(
            tool.calls(),
            0,
            "a tool whose schema needs two fields must never be dispatched on the fast path, \
             however confidently it was chosen"
        );
    }

    // The vendor evaluates `tool` and `needs_external_information` independently, so a confident
    // tool pick that the schema gate disqualifies and a confidently-false need-for-information can
    // co-occur. A confident tool pick must veto NoToolNeeded too, not just block ToolCall.
    #[tokio::test]
    async fn a_confident_schema_disqualified_tool_pick_vetoes_no_tool_needed_too() {
        let fake = Arc::new(FakeLlmRouter::then(
            r#"{"tool_call": null, "reply": "I do not have access to that."}"#,
            r#"{"tool_call": {"name": "some_search", "args": {"query": "q"}}}"#,
        ));
        fake.answer_decide_tool("two_field_tool", 0.95);
        fake.answer_decide_needs_external_info(0.02); // confidently false — must not be enough
        let llm_url = serve(Arc::clone(&fake)).await;
        let tool = Arc::new(FakeToolService::ok("{}"));
        let tool_url = serve_tool(Arc::clone(&tool)).await;
        let executor = LlmTaskExecutor::new(&llm_url, &tool_url).expect("client");
        let config = LlmNodeConfig {
            tool_calling: true,
            available_tools: vec![two_field_tool("two_field_tool")],
            ..Default::default()
        }
        .to_json();

        let output = executor
            .execute("llm", &config, &json!({"question": "q"}), "e:llm:0")
            .await
            .expect("execute");

        assert_eq!(output["tool_call"]["name"], json!("some_search"));
        assert_eq!(tool.calls(), 0, "schema-disqualified, so never dispatched");
        assert_eq!(
            fake.calls(),
            2,
            "a confident tool pick vetoes NoToolNeeded, so today's flow — nudge included — runs"
        );
    }

    #[tokio::test]
    async fn a_confident_pick_of_a_tool_not_in_the_live_catalog_vetoes_no_tool_needed_too() {
        let fake = Arc::new(FakeLlmRouter::then(
            r#"{"tool_call": null, "reply": "I do not have access to that."}"#,
            r#"{"tool_call": {"name": "some_search", "args": {"query": "q"}}}"#,
        ));
        fake.answer_decide_tool("nonexistent_tool", 0.95);
        fake.answer_decide_needs_external_info(0.02);
        let url = serve(Arc::clone(&fake)).await;
        let executor = LlmTaskExecutor::new(&url, UNREACHABLE).expect("client");
        let config = LlmNodeConfig {
            tool_calling: true,
            available_tools: vec![single_field_tool("some_tool", "query")],
            ..Default::default()
        }
        .to_json();

        let output = executor
            .execute("llm", &config, &json!({"question": "q"}), "e:llm:0")
            .await
            .expect("execute");

        assert_eq!(output["tool_call"]["name"], json!("some_search"));
        assert_eq!(
            fake.calls(),
            2,
            "a confident pick naming no tool in the live catalog still vetoes NoToolNeeded"
        );
    }

    #[tokio::test]
    async fn a_low_confidence_tool_pick_does_not_veto_no_tool_needed() {
        let fake = Arc::new(FakeLlmRouter::always(
            r#"{"tool_call": null, "reply": "68"}"#,
        ));
        fake.answer_decide_tool("some_tool", 0.2); // below TOOL_CHOICE_CONFIDENCE_THRESHOLD
        fake.answer_decide_needs_external_info(0.02);
        let url = serve(Arc::clone(&fake)).await;
        let executor = LlmTaskExecutor::new(&url, UNREACHABLE).expect("client");
        let config = LlmNodeConfig {
            tool_calling: true,
            available_tools: vec![single_field_tool("some_tool", "query")],
            ..Default::default()
        }
        .to_json();

        let output = executor
            .execute(
                "llm",
                &config,
                &json!({"question": "what is 17 times 4?"}),
                "e:llm:0",
            )
            .await
            .expect("execute");

        assert_eq!(output["reply"], json!("68"));
        assert_eq!(
            fake.calls(),
            1,
            "a tool mentioned with low confidence must not block the no-tool-needed fast path"
        );
    }

    #[tokio::test]
    async fn a_missing_tool_answer_does_not_veto_no_tool_needed() {
        let fake = Arc::new(FakeLlmRouter::always(
            r#"{"tool_call": null, "reply": "68"}"#,
        ));
        fake.answer_decide_needs_external_info(0.02); // tool left unarmed: no `tool` answer at all
        let url = serve(Arc::clone(&fake)).await;
        let executor = LlmTaskExecutor::new(&url, UNREACHABLE).expect("client");

        let output = executor
            .execute(
                "llm",
                &json!({"tool_calling": true}),
                &json!({"question": "what is 17 times 4?"}),
                "e:llm:0",
            )
            .await
            .expect("execute");

        assert_eq!(output["reply"], json!("68"));
        assert_eq!(
            fake.calls(),
            1,
            "no `tool` answer at all must not block the no-tool-needed fast path"
        );
    }

    // A follow-up's question is not always a question — see MAX_FAST_DISPATCH_QUESTION_CHARS's
    // doc. A confident, schema-eligible tool pick must still not be fast-dispatched when the
    // question itself is multi-line, and must still veto NoToolNeeded (today's flow decides).
    #[tokio::test]
    async fn a_multi_line_question_is_not_fast_dispatched_even_when_the_tool_otherwise_qualifies() {
        let fake = Arc::new(FakeLlmRouter::then(
            r#"{"tool_call": null, "reply": "I do not have access to that."}"#,
            r#"{"tool_call": {"name": "some_search", "args": {"query": "q"}}}"#,
        ));
        fake.answer_decide_tool("some_tool", 0.9);
        fake.answer_decide_needs_external_info(0.02);
        let llm_url = serve(Arc::clone(&fake)).await;
        let tool = Arc::new(FakeToolService::ok("{}"));
        let tool_url = serve_tool(Arc::clone(&tool)).await;
        let executor = LlmTaskExecutor::new(&llm_url, &tool_url).expect("client");
        let config = LlmNodeConfig {
            tool_calling: true,
            available_tools: vec![single_field_tool("some_tool", "query")],
            ..Default::default()
        }
        .to_json();
        let question = "Earlier answer:\nParis\n\nFollow-up: what is its population?";

        let output = executor
            .execute("llm", &config, &json!({"question": question}), "e:llm:0")
            .await
            .expect("execute");

        assert_eq!(output["tool_call"]["name"], json!("some_search"));
        assert_eq!(
            tool.calls(),
            0,
            "a multi-line question must never be fast-dispatched verbatim into a tool's query"
        );
        assert_eq!(
            fake.calls(),
            2,
            "a confident, schema-eligible tool pick still vetoes NoToolNeeded even when the \
             question's own shape blocks fast dispatch — today's flow decides instead"
        );
    }

    #[tokio::test]
    async fn a_question_longer_than_the_fast_dispatch_limit_is_not_fast_dispatched() {
        let fake = Arc::new(FakeLlmRouter::always(
            r#"{"tool_call": null, "reply": "ok"}"#,
        ));
        fake.answer_decide_tool("some_tool", 0.9);
        let llm_url = serve(Arc::clone(&fake)).await;
        let tool = Arc::new(FakeToolService::ok("{}"));
        let tool_url = serve_tool(Arc::clone(&tool)).await;
        let executor = LlmTaskExecutor::new(&llm_url, &tool_url).expect("client");
        let config = LlmNodeConfig {
            tool_calling: true,
            available_tools: vec![single_field_tool("some_tool", "query")],
            ..Default::default()
        }
        .to_json();
        let question = "x".repeat(MAX_FAST_DISPATCH_QUESTION_CHARS + 1);

        executor
            .execute("llm", &config, &json!({"question": question}), "e:llm:0")
            .await
            .expect("execute");

        assert_eq!(
            tool.calls(),
            0,
            "a question over the fast-dispatch length limit must never be sent verbatim"
        );
    }

    #[tokio::test]
    async fn a_failed_decide_falls_through_to_todays_flow_with_the_nudge_enabled() {
        let fake = Arc::new(FakeLlmRouter::then(
            r#"{"tool_call": null, "reply": "I do not have access to that."}"#,
            r#"{"tool_call": {"name": "some_search", "args": {"query": "q"}}}"#,
        ));
        // Neither `answer_decide_tool` nor `answer_decide_needs_external_info` is armed, so
        // `decide` fails — the fail-safe this test exercises.
        let url = serve(Arc::clone(&fake)).await;
        let executor = LlmTaskExecutor::new(&url, UNREACHABLE).expect("client");

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
        assert_eq!(
            fake.decide_calls(),
            1,
            "a Decide attempt must be made and observed to fail, not skipped"
        );
        assert_eq!(
            fake.calls(),
            2,
            "an unavailable Decide falls through to today's flow, nudge included"
        );
    }

    // Real network on loopback (the fake server, the TCP and HTTP/2 handshake), virtual time for
    // the deadline itself — the same idiom as `services/chat/src/intent.rs`'s matching test.
    // Isolated to `decide_fast_path` itself, not the whole `execute` flow: chaining the two further
    // real `Complete` round trips `execute` would need after the fallback onto the same paused
    // clock is exactly the kind of timer/I-O race `tokio::time::pause`'s docs warn about, and the
    // property under test — the deadline bounds `Decide`, not `Complete` — does not need them.
    // `a_failed_decide_falls_through_to_todays_flow_with_the_nudge_enabled` above already proves
    // the fallback's nudge end to end for an outright `Decide` error; `Decide` treats a timeout
    // identically to any other error (see `decide_fast_path`'s single `Err` arm), so between the
    // two, both halves of "any `Decide` error or timeout falls through" are covered.
    #[tokio::test(start_paused = true)]
    async fn a_hung_decide_call_is_abandoned_within_its_own_deadline() {
        let fake = Arc::new(FakeLlmRouter::always(
            r#"{"tool_call": null, "reply": "ok"}"#,
        ));
        fake.hang_decide(Arc::new(tokio::sync::Barrier::new(2)));
        let url = serve(Arc::clone(&fake)).await;
        let executor = LlmTaskExecutor::new(&url, UNREACHABLE).expect("client");
        let config = LlmNodeConfig {
            tool_calling: true,
            ..Default::default()
        };

        let started = tokio::time::Instant::now();
        let outcome = executor
            .decide_fast_path(&config, "which courses exist?")
            .await;
        let elapsed = started.elapsed();

        assert!(
            matches!(outcome, FastPath::Unavailable),
            "a hung Decide must fall back to Unavailable, today's flow with the nudge enabled"
        );
        assert!(
            elapsed >= DECIDE_CALL_TIMEOUT,
            "returned before the deadline even elapsed: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(10),
            "abandoned near the 2s deadline, not a much longer one: {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn the_fast_path_is_only_tried_on_the_first_generative_call_of_a_turn() {
        let fake = Arc::new(FakeLlmRouter::always(
            r#"{"tool_call": null, "reply": "done"}"#,
        ));
        // Armed as if the fast path would fire, to prove it is never even asked here.
        fake.answer_decide_needs_external_info(0.02);
        let url = serve(Arc::clone(&fake)).await;
        let executor = LlmTaskExecutor::new(&url, UNREACHABLE).expect("client");

        executor
            .execute(
                "llm",
                &json!({"tool_calling": true}),
                &json!({"question": "q", "tool_result": [{"ok": true}]}),
                "e:llm:0",
            )
            .await
            .expect("execute");

        assert_eq!(
            fake.decide_calls(),
            0,
            "a later iteration already has tool results and must not re-ask Decide"
        );
    }
}
