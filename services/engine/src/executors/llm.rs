// Runs an llm node by calling llm-router and parsing its reply.

use std::sync::LazyLock;
use std::time::Duration;

use buffa::EnumValue;
use common::agent_replies::{
    CLARIFY_OPENING, COMPOSE_FAILURE, DECLINE_OPENING, NOTHING_CONNECTED, PROMISE_REFUSED,
    UNGROUNDED_REFUSED,
};
use common::execution_input::ExecutionInput;
use common::proto::llm_router::v1::{
    Answer, Choice, ChoiceAnswer, ChoiceOption, CompleteRequest, CompleteResponse, DecideRequest,
    DecideResponse, LlmRouterServiceClient, Noul, QualityTier, Question, ResponseFormat, Sampling,
    SystemOneServiceClient, answer::Answer as Given,
};
use common::proto::tools::v1::{ExecuteRequest, ExecuteResponse, ExecuteStatus, ToolServiceClient};
use connectrpc::client::HttpClient;
use engine_core::{
    FAST_TOOL_CALL_FIELD, LlmOutput, PRIOR_MATERIAL_STATE_KEY, RENDERED_TOOL_RESULTS,
    TOOL_RESULT_ERROR_KEY, TOOL_RESULT_STATE_KEY, TaskError, TaskExecutor, ToolCall,
};
use serde_json::{Value, json};

use chrono::Utc;

use crate::dispatch::NodeKind;

const DEFAULT_SYSTEM_PROMPT: &str = "You are a helpful assistant.";
const TOOL_SLUG_PLACEHOLDER: &str = "<tool slug>";
const ARGUMENT_NAME_PLACEHOLDER: &str = "<argument name>";
const ARGUMENT_VALUE_PLACEHOLDER: &str = "<argument value>";
const ANSWER_PLACEHOLDER: &str = "<your answer>";
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Loop,
    ComposeOnly,
    FromMaterial,
}

static COMPOSE_ONLY_INSTRUCTIONS: LazyLock<String> = LazyLock::new(|| {
    format!(
        r#"
Call one of the tools below. Respond with exactly this JSON and nothing else:
{}
Argument values come from the message itself, or from material already shown to you. Never supply
one of your own.
Do not answer from your own knowledge, and do not describe what you are about to do — a reply
without a tool call is not accepted here.
"#,
        shape_of(LlmOutput {
            tool_call: serde_json::to_value(ToolCall {
                name: TOOL_SLUG_PLACEHOLDER.to_owned(),
                args: json!({ARGUMENT_NAME_PLACEHOLDER: ARGUMENT_VALUE_PLACEHOLDER}),
            })
            .expect("a ToolCall always serializes"),
            reply: Value::Null,
        }),
    )
});

static FROM_MATERIAL_INSTRUCTIONS: LazyLock<String> = LazyLock::new(|| {
    format!(
        r#"
Answer from the material below, which this system looked up earlier in this conversation. Use
only what is there — including what was asked for, which the material records beside each result.
Respond with exactly this JSON and nothing else:
{}
"#,
        shape_of(LlmOutput {
            tool_call: Value::Null,
            reply: Value::String(ANSWER_PLACEHOLDER.to_owned()),
        }),
    )
});

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
const CALL_TIMEOUT: Duration = Duration::from_secs(65);
const RETRY_ATTEMPTS: u32 = 3;
const RETRY_BASE_DELAY: Duration = Duration::from_millis(200);
const MAX_CHARS_PER_RENDERED_TOOL_RESULT: usize = 12_000;
const TRUNCATION_MARK: &str = "… (truncated)";

const DECIDE_CALL_TIMEOUT: Duration = Duration::from_secs(2);
const TOOL_CALL_TIMEOUT: Duration = Duration::from_secs(60);

const PROMISE_ID: &str = "promise";
const PROMISE_ENOUGH_TO_REFUSE: f64 = 0.5;
const PROMISE_INSTRUCTIONS: &str = "Does this text undertake to do something, rather than report \
something already done or known?";
const PROMISE_WHEN_TRUE: &str = "it says what it is about to do, will do, or needs to do — work \
that has not happened yet";
const PROMISE_WHEN_FALSE: &str = "it states what is the case, gives a value, answers, asks the \
person something, or says plainly that it could not find out";
const PROMISE_CORRECTION: &str = r#" It undertook to do something rather than
reporting what came back. Nothing runs after your reply. Answer from what is already in front of
you.
"#;

const POINTED_AT_ID: &str = "pointed_at";
const POINTED_AT_ENOUGH_TO_DISPATCH: f64 = 0.8;
const BELOW_EVERY_THRESHOLD: f64 = 0.0;
const FETCHED_AT_FIELD: &str = "fetched_at";
const CARRIED_MATERIAL_GOES_STALE_AFTER: chrono::TimeDelta = chrono::TimeDelta::minutes(10);
const POINTED_AT_INSTRUCTIONS: &str = "Does `message` refer to `value`, by that name or by any \
description that can only mean it?";
const POINTED_AT_WHEN_TRUE: &str = "the message points at this one thing, whether by naming it or \
by describing it unambiguously";
const POINTED_AT_WHEN_FALSE: &str =
    "the message does not point at this thing at all, or points at it only vaguely";

const GROUNDED_ID: &str = "grounded";
const GROUNDED_ENOUGH_TO_SEND: f64 = 0.5;
const GROUNDED_INSTRUCTIONS: &str = "Is every specific value in the reply present in the material?";
const GROUNDED_WHEN_TRUE: &str =
    "every name, number and fact in the reply can be found in the material";
const GROUNDED_WHEN_FALSE: &str =
    "the reply states a name, number or fact the material does not contain";
const UNGROUNDED_CORRECTION: &str = r#" Nothing in what came back says it.
Write the reply again using only the values in front of you, and do not repeat the refused words.
The question names a subject; what came back holds what was measured about it, and the measurement
is the answer — never the subject's own name. If what came back does not hold the answer, say so.
"#;
const TOOL_QUESTION_ID: &str = "tool";
const NO_TOOL_OPTION: &str = "none";
const PRIOR_OPTION: &str = "already_found";
const PRIOR_DESCRIPTION: &str = "the material already shown above. Choose this when the answer is \
somewhere in that material — including what was asked for, which the material records beside each \
result — so nothing new needs looking up";

const TOOL_QUESTION_INSTRUCTIONS: &str = "Which of these sources can answer this question? Choose \
the one whose material covers what is being asked. Choose the option for none of them when none \
does.";
const DECLINE_CLOSING: &str = "Which of those did you mean?";
const MISSING_VALUE_FALLBACK: &str = "to know what to look it up for";
const NO_TOOL_DESCRIPTION: &str =
    "no source listed here holds what this question asks for, or the message asks for nothing";

const TOOL_CHOICE_CONFIDENT_ENOUGH_TO_ACT_ON: f64 = 0.5;
const MAX_CHARS_SENT_TO_A_TOOL_VERBATIM: usize = 200;

const VALUE_QUESTION_PREFIX: &str = "value::";
const VALUE_STATED_PREFIX: &str = "stated::";
const VALUE_ONE_ONLY_PREFIX: &str = "one_only::";
const NO_VALUE_OPTION: &str = "none";
const VALUE_CHOICE_CONFIDENT_ENOUGH_TO_DISPATCH: f64 = 0.6;
const VALUE_STATED_ENOUGH_TO_DISPATCH: f64 = 0.5;
const VALUE_ONE_ONLY_ENOUGH_TO_DISPATCH: f64 = 0.5;
const MAX_SPAN_WORDS: usize = 3;
const DECIDER_OPTION_CEILING: usize = 255;
const MAX_SPAN_OPTIONS: usize = 200;
const MAX_OPTION_NAME_BYTES: usize = 64;
const _: () = assert!(MAX_SPAN_OPTIONS < DECIDER_OPTION_CEILING);
const MAX_ARGUMENTS_ASKED_ABOUT_AT_ONCE: usize = 4;

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
    #[serde(default)]
    pub title: String,
    pub description: String,
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
    build_prompt_for(config, state, Mode::Loop)
}

pub fn build_prompt_for(config: &LlmNodeConfig, state: &Value, mode: Mode) -> (String, String) {
    let mut system = config
        .system_prompt
        .clone()
        .unwrap_or_else(|| DEFAULT_SYSTEM_PROMPT.to_owned());
    if config.tool_calling {
        system.push_str(match mode {
            Mode::Loop => &TOOL_CALLING_INSTRUCTIONS,
            Mode::ComposeOnly => &COMPOSE_ONLY_INSTRUCTIONS,
            Mode::FromMaterial => &FROM_MATERIAL_INSTRUCTIONS,
        });
    }
    let answer_is_already_in_hand = mode == Mode::FromMaterial;
    if !config.available_tools.is_empty() && !answer_is_already_in_hand {
        system.push_str("\n\nAvailable tools:\n");
        for tool in &config.available_tools {
            system.push_str(&format!("- {}: {}\n", tool.name, tool.description));
        }
    }
    if config.tool_calling && !answer_is_already_in_hand {
        system.push_str(TOOL_USE_POLICY);
    }
    let there_is_a_reply_to_style = mode != Mode::ComposeOnly;
    if there_is_a_reply_to_style {
        system.push_str(FINAL_ANSWER_STYLE);
    }

    let mut user = ExecutionInput::in_state(state).question;
    for (index, result) in rendered_tool_results(state) {
        user.push_str(&render_tool_result(index, result));
    }
    (system, user)
}

fn rendered_tool_results(state: &Value) -> impl Iterator<Item = (usize, &Value)> {
    let prior = state
        .get(PRIOR_MATERIAL_STATE_KEY)
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    let fetched = state
        .get(TOOL_RESULT_STATE_KEY)
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    let carried_then_fetched: Vec<&Value> = prior.iter().chain(fetched.iter()).collect();
    let first_rendered = carried_then_fetched
        .len()
        .saturating_sub(RENDERED_TOOL_RESULTS);
    carried_then_fetched
        .into_iter()
        .enumerate()
        .skip(first_rendered)
}

fn still_fresh(records: Option<&Value>) -> Value {
    let Some(records) = records.and_then(Value::as_array) else {
        return Value::Null;
    };
    let now = Utc::now();
    Value::Array(
        records
            .iter()
            .filter(|record| {
                record
                    .get(FETCHED_AT_FIELD)
                    .and_then(Value::as_str)
                    .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
                    .is_some_and(|at| {
                        now - at.with_timezone(&Utc) < CARRIED_MATERIAL_GOES_STALE_AFTER
                    })
            })
            .cloned()
            .collect::<Vec<_>>(),
    )
}

fn rendered_prior_material(state: &Value) -> Option<String> {
    let records = state.get(PRIOR_MATERIAL_STATE_KEY)?.as_array()?;
    if records.is_empty() {
        return None;
    }
    let first = records.len().saturating_sub(RENDERED_TOOL_RESULTS);
    Some(
        records
            .iter()
            .enumerate()
            .skip(first)
            .map(|(index, record)| render_tool_result(index, record))
            .collect(),
    )
}

fn render_tool_result(index: usize, record: &Value) -> String {
    let bare_output_without_the_call = (None, record);
    let (asked, result) = match record.get("result") {
        Some(result) => (record.get("args"), result),
        None => bare_output_without_the_call,
    };
    let asked = asked.map_or_else(String::new, |args| format!(" (asked for {args})"));
    match result.get(TOOL_RESULT_ERROR_KEY).and_then(Value::as_str) {
        Some(error) => format!(
            "\n\nTool result {index}{asked} — ERROR: {}",
            truncated(error)
        ),
        None => format!(
            "\n\nTool result {index}{asked}: {}",
            truncated(&result.to_string())
        ),
    }
}

fn truncated(text: &str) -> String {
    if text.chars().count() <= MAX_CHARS_PER_RENDERED_TOOL_RESULT {
        return text.to_owned();
    }
    let kept: String = text
        .chars()
        .take(MAX_CHARS_PER_RENDERED_TOOL_RESULT)
        .collect();
    format!("{kept}{TRUNCATION_MARK}")
}

#[must_use]
pub fn parse_llm_output(content: &str) -> Value {
    let parsed = serde_json::from_str::<LlmOutput>(content).unwrap_or_default();
    let output = match (parsed.is_blank(), first_envelope(content)) {
        (false, _) => parsed,
        (true, Some(recovered)) => recovered,
        (true, None) => LlmOutput {
            tool_call: Value::Null,
            reply: Value::String(content.to_owned()),
        },
    };
    serde_json::to_value(output).expect("an LlmOutput always serializes")
}

fn first_envelope(content: &str) -> Option<LlmOutput> {
    content
        .char_indices()
        .filter(|(_, c)| *c == '{')
        .find_map(|(at, _)| -> Option<LlmOutput> {
            let parsed = serde_json::Deserializer::from_str(&content[at..])
                .into_iter::<LlmOutput>()
                .next()?
                .ok()?;
            (!parsed.is_blank()).then_some(parsed)
        })
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
        let material = everything_fetched_so_far(state);

        Ok(self.checked_against(&request, &material, output).await)
    }
}

fn everything_fetched_so_far(state: &Value) -> Value {
    let of = |key: &str| {
        state
            .get(key)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    let mut records = of(PRIOR_MATERIAL_STATE_KEY);
    records.extend(of(TOOL_RESULT_STATE_KEY));
    Value::Array(records)
}

fn complete_request(config: &LlmNodeConfig, state: &Value) -> CompleteRequest {
    complete_request_for(config, state, Mode::Loop)
}

fn complete_request_for(config: &LlmNodeConfig, state: &Value, mode: Mode) -> CompleteRequest {
    let (system_prompt, user_prompt) = build_prompt_for(config, state, mode);
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

enum FastPath {
    ToolCall { call: ToolCall, decide_usage: Value },
    ComposeOnly { tool: String, decide_usage: Value },
    FromMaterial { decide_usage: Value },
    Decline { decide_usage: Value },
}

impl LlmTaskExecutor {
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
        let prior = rendered_prior_material(state);
        match self
            .decide_fast_path(config, &question, prior.as_deref())
            .await
        {
            FastPath::ToolCall { call, decide_usage } => Some(
                self.run_fast_tool_call(config, state, &call, &decide_usage, idempotency_key)
                    .await,
            ),
            FastPath::ComposeOnly { tool, decide_usage } => Some(
                self.compose_tool_call(config, state, &tool, &decide_usage, idempotency_key)
                    .await,
            ),
            FastPath::FromMaterial { decide_usage } => Some(
                self.answer_from_material(config, state, &decide_usage)
                    .await,
            ),
            FastPath::Decline { decide_usage } => Some(Ok(decline(config, &decide_usage))),
        }
    }

    async fn answer_from_material(
        &self,
        config: &LlmNodeConfig,
        state: &Value,
        decide_usage: &Value,
    ) -> Result<Value, TaskError> {
        let material = still_fresh(state.get(PRIOR_MATERIAL_STATE_KEY));
        if material.as_array().is_none_or(Vec::is_empty) {
            tracing::info!("what the earlier turn found has gone stale, declining instead");
            return Ok(decline(config, decide_usage));
        }
        let request = complete_request_for(config, state, Mode::FromMaterial);
        let output = self.complete_with_retries(&request).await?;
        let output = self.checked_against(&request, &material, output).await;
        Ok(merge_usage(decide_usage, without_tool_call(output)))
    }

    async fn checked_against(
        &self,
        request: &CompleteRequest,
        material: &Value,
        output: Value,
    ) -> Value {
        let Some(faults) = self.faults_in(material, &output).await else {
            return output;
        };
        let output = merge_usage(&faults.spent, output);
        if faults.sound() {
            return output;
        }
        tracing::info!(
            ?faults,
            "the reply did not hold up against what came back, asking again"
        );
        let Ok(second) = self.asked_again_with(request, &faults, &output).await else {
            return refused(output, faults.refusal());
        };
        let second = merge_usage(&output, second);
        match self.faults_in(material, &second).await {
            Some(again) if !again.sound() => {
                tracing::warn!(
                    ?again,
                    "the second reply did not hold up either, answering plainly"
                );
                refused(merge_usage(&again.spent, second), again.refusal())
            }
            Some(again) => merge_usage(&again.spent, second),
            None => second,
        }
    }

    async fn asked_again_with(
        &self,
        request: &CompleteRequest,
        faults: &Faults,
        rejected_in: &Value,
    ) -> Result<Value, TaskError> {
        let rejected = rejected_in
            .get("reply")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let mut asked_again_with_the_fault_stated = request.clone();
        asked_again_with_the_fault_stated
            .system_prompt
            .push_str(&faults.correction(rejected));
        self.complete_with_retries(&asked_again_with_the_fault_stated)
            .await
    }

    async fn composed(
        &self,
        config: &LlmNodeConfig,
        state: &Value,
        tool: &str,
    ) -> Result<Value, TaskError> {
        let request = complete_request_for(config, state, Mode::ComposeOnly);
        let spent = self.complete_with_retries(&request).await?;
        if composed_call(&spent).is_some() {
            return Ok(spent);
        }
        tracing::info!(
            tool,
            "compose-only reply carried no tool call, asking once more"
        );
        let mut asked_again_with_the_refusal_stated = request;
        asked_again_with_the_refusal_stated
            .system_prompt
            .push_str(COMPOSE_REFUSAL);
        let second = self
            .complete_with_retries(&asked_again_with_the_refusal_stated)
            .await?;
        Ok(merge_usage(&spent, second))
    }

    async fn asked_for_instead(
        &self,
        config: &LlmNodeConfig,
        state: &Value,
        call: &ToolCall,
    ) -> Option<Value> {
        let asked = self.not_pointed_at(state, call).await?;
        tracing::info!(
            tool = call.name,
            value = %asked,
            "the message points at no such value, asking for it"
        );
        Some(clarify(self.schema_of(config, &call.name), &Value::Null))
    }

    async fn decided_once_more_on_error(&self, request: DecideRequest) -> Option<DecideResponse> {
        let second = request.clone();
        match self.decider.decide(request).await {
            Ok(response) => return Some(response.into_owned()),
            Err(error) => tracing::info!(%error, "the check did not answer, asking once more"),
        }
        match self.decider.decide(second).await {
            Ok(response) => Some(response.into_owned()),
            Err(error) => {
                tracing::warn!(%error, "the check did not answer twice");
                None
            }
        }
    }

    async fn not_pointed_at(&self, state: &Value, call: &ToolCall) -> Option<String> {
        let question = ExecutionInput::in_state(state).question;
        let Some(request) = pointed_at_request(&question, &call.args) else {
            return Some(call.args.to_string());
        };
        let Some(decided) = self.decided_once_more_on_error(request).await else {
            tracing::warn!("could not check the composed value, asking for it instead");
            return Some(call.args.to_string());
        };
        let pointed = find_noul(&decided.answers, POINTED_AT_ID).unwrap_or(BELOW_EVERY_THRESHOLD);
        (pointed < POINTED_AT_ENOUGH_TO_DISPATCH).then(|| call.args.to_string())
    }

    fn schema_of<'a>(&self, config: &'a LlmNodeConfig, name: &str) -> &'a str {
        config
            .available_tools
            .iter()
            .find(|tool| tool.name == name)
            .map_or("", |tool| tool.input_schema_json.as_str())
    }

    async fn faults_in(&self, material: &Value, output: &Value) -> Option<Faults> {
        let reply = output
            .get("reply")
            .and_then(Value::as_str)
            .filter(|reply| {
                !reply.is_empty() && output.get("tool_call").is_none_or(Value::is_null)
            })?;
        let request = reply_check_request(material, reply)?;
        let decided = match self.decider.decide(request).await {
            Ok(response) => response.into_owned(),
            Err(error) => {
                tracing::warn!(%error, "could not check the reply, leaving it as it is");
                return None;
            }
        };
        Some(Faults {
            promises: find_noul(&decided.answers, PROMISE_ID)
                .is_some_and(|value| value > PROMISE_ENOUGH_TO_REFUSE),
            ungrounded: find_noul(&decided.answers, GROUNDED_ID)
                .is_some_and(|value| value < GROUNDED_ENOUGH_TO_SEND),
            spent: decide_usage_value(&decided),
        })
    }

    async fn compose_tool_call(
        &self,
        config: &LlmNodeConfig,
        state: &Value,
        tool: &str,
        decide_usage: &Value,
        idempotency_key: &str,
    ) -> Result<Value, TaskError> {
        let spent = self.composed(config, state, tool).await?;
        let Some(call) = composed_call(&spent).filter(|call| in_catalog(config, call)) else {
            tracing::warn!(
                tool,
                "compose-only produced no tool call twice, answering plainly"
            );
            return Ok(merge_usage(
                decide_usage,
                merge_usage(
                    &spent,
                    json!({ "tool_call": Value::Null, "reply": COMPOSE_FAILURE }),
                ),
            ));
        };
        if let Some(asking) = self.asked_for_instead(config, state, &call).await {
            return Ok(merge_usage(decide_usage, merge_usage(&spent, asking)));
        }
        let result = self.dispatch_tool_fast(&call, idempotency_key).await;
        let state_with_result =
            state_with_extra_tool_result(state, tool_record(&call, result.clone()));
        let request = complete_request_for(config, &state_with_result, Mode::Loop);
        let output = self.complete_with_retries(&request).await?;
        let output = self.checked_against(&request, &result, output).await;
        let output = merge_usage(decide_usage, merge_usage(&spent, output));
        Ok(attach_fast_tool_call(output, &call, result))
    }

    async fn run_fast_tool_call(
        &self,
        config: &LlmNodeConfig,
        state: &Value,
        call: &ToolCall,
        decide_usage: &Value,
        idempotency_key: &str,
    ) -> Result<Value, TaskError> {
        let result = self.dispatch_tool_fast(call, idempotency_key).await;
        let state_with_result =
            state_with_extra_tool_result(state, tool_record(call, result.clone()));
        let request = complete_request_for(config, &state_with_result, Mode::Loop);
        let output = self.complete_with_retries(&request).await?;
        let output = self.checked_against(&request, &result, output).await;
        let output = merge_usage(decide_usage, output);
        Ok(attach_fast_tool_call(output, call, result))
    }

    async fn decide_fast_path(
        &self,
        config: &LlmNodeConfig,
        question: &str,
        rendered_prior: Option<&str>,
    ) -> FastPath {
        let Some(request) = decide_request(question, &config.available_tools, rendered_prior)
        else {
            return FastPath::Decline {
                decide_usage: Value::Null,
            };
        };
        let decided = match self.decider.decide(request).await {
            Ok(response) => response.into_owned(),
            Err(error) => {
                tracing::warn!(
                    %error,
                    "the scope decision did not answer, so the turn declines"
                );
                return FastPath::Decline {
                    decide_usage: Value::Null,
                };
            }
        };
        let decide_usage = decide_usage_value(&decided);

        if let Some(choice) = confident_tool_choice(&decided.answers) {
            if choice.choice == PRIOR_OPTION {
                return FastPath::FromMaterial { decide_usage };
            }
            return match fast_dispatchable_tool_call(
                choice,
                &config.available_tools,
                question,
                &decided.answers,
            ) {
                Some(call) => FastPath::ToolCall { call, decide_usage },
                None if names_a_live_tool(choice, &config.available_tools) => {
                    FastPath::ComposeOnly {
                        tool: choice.choice.clone(),
                        decide_usage,
                    }
                }
                None => FastPath::Decline { decide_usage },
            };
        }
        FastPath::Decline { decide_usage }
    }

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

fn decide_usage_value(decided: &DecideResponse) -> Value {
    json!({"usage": {"tokens_in": decided.tokens_in, "tokens_out": decided.tokens_out}})
}

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

fn tool_record(call: &ToolCall, result: Value) -> Value {
    json!({
        "name": call.name,
        "args": call.args,
        "result": result,
        FETCHED_AT_FIELD: Utc::now().to_rfc3339(),
    })
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

fn decide_request(
    question: &str,
    available_tools: &[CatalogEntry],
    rendered_prior: Option<&str>,
) -> Option<DecideRequest> {
    let state = match rendered_prior {
        Some(rendered) => json!({"message": question, "already_found": rendered}),
        None => json!(question),
    };
    let mut request: DecideRequest = serde_json::from_value(json!({ "state": state })).ok()?;
    request.questions = vec![tool_question(available_tools, rendered_prior.is_some())?];
    request
        .questions
        .extend(decidable_questions(question, available_tools));
    Some(request)
}

fn spans(message: &str) -> Vec<String> {
    let words: Vec<&str> = message
        .split_whitespace()
        .map(|word| word.trim_matches(|c: char| !c.is_alphanumeric()))
        .filter(|word| !word.is_empty())
        .collect();
    let mut found: Vec<String> = Vec::new();
    for size in 1..=MAX_SPAN_WORDS {
        for run in words.windows(size) {
            if found.len() >= MAX_SPAN_OPTIONS {
                return found;
            }
            let candidate = run.join(" ");
            let collides_with_the_sentinel = candidate.eq_ignore_ascii_case(NO_VALUE_OPTION);
            let too_long_to_offer = candidate.len() > MAX_OPTION_NAME_BYTES;
            if collides_with_the_sentinel || too_long_to_offer || found.contains(&candidate) {
                continue;
            }
            found.push(candidate);
        }
    }
    found
}

fn decidable_argument(input_schema_json: &str) -> Option<(String, String)> {
    let (field, description, _) = decidable_field(input_schema_json)?;
    Some((field, description))
}

enum Options {
    Closed(Vec<(String, Option<String>)>),
    Spans,
}

fn decidable_field(input_schema_json: &str) -> Option<(String, String, Options)> {
    let schema: Value = serde_json::from_str(input_schema_json).ok()?;
    let required = schema.get("required")?.as_array()?;
    let [only] = required.as_slice() else {
        return None;
    };
    let field = only.as_str()?;
    let property = schema.get("properties")?.get(field)?;
    if property.get("type").and_then(Value::as_str) != Some("string") {
        return None;
    }
    let options = closed_list(property).map_or_else(
        || common::tool_schema::value_in_message(property).then_some(Options::Spans),
        |listed| Some(Options::Closed(listed)),
    )?;
    let description = property
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or(field);
    Some((field.to_owned(), description.to_owned(), options))
}

fn closed_list(property: &Value) -> Option<Vec<(String, Option<String>)>> {
    if let Some(variants) = property.get("oneOf").and_then(Value::as_array) {
        let listed: Vec<(String, Option<String>)> = variants
            .iter()
            .filter_map(|variant| {
                let value = variant.get("const")?.as_str()?.to_owned();
                let line = variant
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                Some((value, line))
            })
            .collect();
        return (!listed.is_empty()).then_some(listed);
    }
    let listed: Vec<(String, Option<String>)> = property
        .get("enum")?
        .as_array()?
        .iter()
        .filter_map(|value| Some((value.as_str()?.to_owned(), None)))
        .collect();
    (!listed.is_empty()).then_some(listed)
}

fn decidable_questions(message: &str, available_tools: &[CatalogEntry]) -> Vec<Question> {
    let candidates = spans(message);
    available_tools
        .iter()
        .filter_map(|tool| {
            let (_, description, options) = decidable_field(&tool.input_schema_json)?;
            let listed = match options {
                Options::Closed(listed) => listed,
                Options::Spans => candidates.iter().map(|span| (span.clone(), None)).collect(),
            };
            (!listed.is_empty() && listed.len() <= MAX_SPAN_OPTIONS).then_some((
                tool.name.as_str(),
                description,
                listed,
            ))
        })
        .take(MAX_ARGUMENTS_ASKED_ABOUT_AT_ONCE)
        .flat_map(|(name, description, listed)| {
            question_set_for(name, &description, &listed).unwrap_or_default()
        })
        .collect()
}

fn question_set_for(
    tool: &str,
    description: &str,
    candidates: &[(String, Option<String>)],
) -> Option<Vec<Question>> {
    let mut options: Vec<ChoiceOption> = candidates
        .iter()
        .map(|(value, line)| ChoiceOption {
            name: value.clone(),
            description: line.clone(),
            ..Default::default()
        })
        .collect();
    options.push(ChoiceOption {
        name: NO_VALUE_OPTION.to_owned(),
        description: Some("no option is that value".to_owned()),
        ..Default::default()
    });

    let mut value = instructed(
        &format!("{VALUE_QUESTION_PREFIX}{tool}"),
        json!({"field": description, "question": "Which option is the value of `field` in `message`?"}),
    )?;
    value.kind = Choice {
        options,
        ..Default::default()
    }
    .into();

    let mut stated = instructed(
        &format!("{VALUE_STATED_PREFIX}{tool}"),
        json!(format!("Does `message` name {description} outright?")),
    )?;
    stated.kind = Noul {
        when_true: Some("the message names it in so many words".to_owned()),
        when_false: Some(
            "it is implied, absent, or would have to be worked out from something else".to_owned(),
        ),
        ..Default::default()
    }
    .into();

    let mut one_only = instructed(
        &format!("{VALUE_ONE_ONLY_PREFIX}{tool}"),
        json!(format!(
            "Does `message` name exactly one {description}, rather than several?"
        )),
    )?;
    one_only.kind = Noul {
        when_true: Some("exactly one".to_owned()),
        when_false: Some("two or more, or a list".to_owned()),
        ..Default::default()
    }
    .into();

    Some(vec![value, stated, one_only])
}

fn picked_value(answers: &[Answer], tool: &str) -> Option<String> {
    let choice = find_choice(answers, &format!("{VALUE_QUESTION_PREFIX}{tool}"))?;
    let a_value_was_chosen = choice.choice != NO_VALUE_OPTION
        && choice.confidence >= VALUE_CHOICE_CONFIDENT_ENOUGH_TO_DISPATCH;
    let the_message_states_it = find_noul(answers, &format!("{VALUE_STATED_PREFIX}{tool}"))?
        >= VALUE_STATED_ENOUGH_TO_DISPATCH;
    let the_message_states_only_one =
        find_noul(answers, &format!("{VALUE_ONE_ONLY_PREFIX}{tool}"))?
            >= VALUE_ONE_ONLY_ENOUGH_TO_DISPATCH;
    (a_value_was_chosen && the_message_states_it && the_message_states_only_one)
        .then(|| choice.choice.clone())
}

fn instructed(id: &str, instructions: Value) -> Option<Question> {
    let mut question: Question =
        serde_json::from_value(json!({ "instructions": instructions })).ok()?;
    question.id = id.to_owned();
    Some(question)
}

fn reply_check_request(material: &Value, reply: &str) -> Option<DecideRequest> {
    let state = json!({"material": material, "reply": reply});
    let mut request: DecideRequest = serde_json::from_value(json!({ "state": state })).ok()?;
    request.questions = vec![
        noul_question(
            PROMISE_ID,
            PROMISE_INSTRUCTIONS,
            PROMISE_WHEN_TRUE,
            PROMISE_WHEN_FALSE,
        )?,
        noul_question(
            GROUNDED_ID,
            GROUNDED_INSTRUCTIONS,
            GROUNDED_WHEN_TRUE,
            GROUNDED_WHEN_FALSE,
        )?,
    ];
    Some(request)
}

fn noul_question(
    id: &str,
    instructions: &str,
    when_true: &str,
    when_false: &str,
) -> Option<Question> {
    let mut question = instructed(id, json!(instructions))?;
    question.kind = Noul {
        when_true: Some(when_true.to_owned()),
        when_false: Some(when_false.to_owned()),
        ..Default::default()
    }
    .into();
    Some(question)
}

#[derive(Clone, Debug)]
struct Faults {
    promises: bool,
    ungrounded: bool,
    spent: Value,
}

impl Faults {
    fn sound(&self) -> bool {
        !self.promises && !self.ungrounded
    }

    fn correction(&self, rejected: &str) -> String {
        let fault = if self.ungrounded {
            UNGROUNDED_CORRECTION
        } else {
            PROMISE_CORRECTION
        };
        format!("\n\nYour previous reply was refused: \"{rejected}\".{fault}")
    }

    fn refusal(&self) -> &'static str {
        if self.ungrounded {
            UNGROUNDED_REFUSED
        } else {
            PROMISE_REFUSED
        }
    }
}

fn pointed_at_request(question: &str, args: &Value) -> Option<DecideRequest> {
    let state = json!({"message": question, "value": args});
    let mut request: DecideRequest = serde_json::from_value(json!({ "state": state })).ok()?;
    request.questions = vec![noul_question(
        POINTED_AT_ID,
        POINTED_AT_INSTRUCTIONS,
        POINTED_AT_WHEN_TRUE,
        POINTED_AT_WHEN_FALSE,
    )?];
    Some(request)
}

fn clarify(input_schema_json: &str, decide_usage: &Value) -> Value {
    let wanted = decidable_argument(input_schema_json)
        .map(|(_, description)| description)
        .unwrap_or_else(|| MISSING_VALUE_FALLBACK.to_owned());
    let reply = format!("{CLARIFY_OPENING}{wanted}.");
    merge_usage(
        decide_usage,
        json!({"tool_call": Value::Null, "reply": reply}),
    )
}

fn refused(mut output: Value, reply: &str) -> Value {
    if let Some(object) = output.as_object_mut() {
        object.insert("reply".to_owned(), json!(reply));
    }
    output
}

fn tool_question(available_tools: &[CatalogEntry], has_prior: bool) -> Option<Question> {
    let mut options: Vec<ChoiceOption> = available_tools
        .iter()
        .map(|tool| ChoiceOption {
            name: tool.name.clone(),
            description: (!tool.description.is_empty()).then(|| tool.description.clone()),
            ..Default::default()
        })
        .collect();
    if has_prior {
        options.push(ChoiceOption {
            name: PRIOR_OPTION.to_owned(),
            description: Some(PRIOR_DESCRIPTION.to_owned()),
            ..Default::default()
        });
    }
    options.push(ChoiceOption {
        name: NO_TOOL_OPTION.to_owned(),
        description: Some(NO_TOOL_DESCRIPTION.to_owned()),
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

fn confident_tool_choice(answers: &[Answer]) -> Option<&ChoiceAnswer> {
    find_choice(answers, TOOL_QUESTION_ID).filter(|choice| {
        choice.choice != NO_TOOL_OPTION
            && choice.confidence >= TOOL_CHOICE_CONFIDENT_ENOUGH_TO_ACT_ON
    })
}

fn fast_dispatchable_tool_call(
    choice: &ChoiceAnswer,
    available_tools: &[CatalogEntry],
    question: &str,
    answers: &[Answer],
) -> Option<ToolCall> {
    let tool = available_tools
        .iter()
        .find(|tool| tool.name == choice.choice)?;
    call_with_the_picked_value(tool, answers)
        .or_else(|| call_with_the_question_verbatim(tool, question))
}

fn call_with_the_picked_value(tool: &CatalogEntry, answers: &[Answer]) -> Option<ToolCall> {
    let (field, _) = decidable_argument(&tool.input_schema_json)?;
    let value = picked_value(answers, &tool.name)?;
    Some(ToolCall {
        name: tool.name.clone(),
        args: json!({ field: value }),
    })
}

fn call_with_the_question_verbatim(tool: &CatalogEntry, question: &str) -> Option<ToolCall> {
    if !safe_to_send_verbatim(question) {
        return None;
    }
    let field = single_required_free_text_field(&tool.input_schema_json)?;
    Some(ToolCall {
        name: tool.name.clone(),
        args: json!({ field: question }),
    })
}

fn safe_to_send_verbatim(question: &str) -> bool {
    let trimmed = question.trim();
    !trimmed.is_empty()
        && !trimmed.contains('\n')
        && trimmed.chars().count() <= MAX_CHARS_SENT_TO_A_TOOL_VERBATIM
}

fn single_required_free_text_field(input_schema_json: &str) -> Option<String> {
    let schema: Value = serde_json::from_str(input_schema_json).ok()?;
    let required = schema.get("required")?.as_array()?;
    let [only] = required.as_slice() else {
        return None;
    };
    let field = only.as_str()?;
    let property = schema.get("properties")?.get(field)?;
    let is_string = property.get("type").and_then(Value::as_str) == Some("string");
    (is_string && common::tool_schema::accepts_free_text(property)).then(|| field.to_owned())
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

const COMPOSE_REFUSAL: &str = r#"

Your previous response was refused: it carried no tool call. Saying that you will look something
up does nothing — nothing runs after your reply. Call the tool now.
"#;

fn names_a_live_tool(choice: &ChoiceAnswer, available_tools: &[CatalogEntry]) -> bool {
    available_tools
        .iter()
        .any(|tool| tool.name == choice.choice)
}

fn decline(config: &LlmNodeConfig, decide_usage: &Value) -> Value {
    let names = sources_named_once_each(config);
    let reply = if names.is_empty() {
        NOTHING_CONNECTED.to_owned()
    } else {
        format!(
            "{DECLINE_OPENING}{}. {DECLINE_CLOSING}",
            listed_in_a_sentence(&names)
        )
    };
    merge_usage(
        decide_usage,
        json!({"tool_call": Value::Null, "reply": reply}),
    )
}

fn sources_named_once_each(config: &LlmNodeConfig) -> Vec<&str> {
    let mut names: Vec<&str> = Vec::new();
    for tool in &config.available_tools {
        let name = as_the_operator_named_it(tool);
        if !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

fn as_the_operator_named_it(tool: &CatalogEntry) -> &str {
    Some(tool.title.as_str())
        .filter(|title| !title.is_empty())
        .unwrap_or(tool.name.as_str())
}

fn listed_in_a_sentence(names: &[&str]) -> String {
    match names {
        [] => String::new(),
        [only] => (*only).to_owned(),
        [rest @ .., last] => format!("{}, and {last}", rest.join(", ")),
    }
}

fn without_tool_call(mut output: Value) -> Value {
    if let Some(object) = output.as_object_mut() {
        object.insert("tool_call".to_owned(), Value::Null);
    }
    output
}

fn in_catalog(config: &LlmNodeConfig, call: &ToolCall) -> bool {
    let known = config
        .available_tools
        .iter()
        .any(|entry| entry.name == call.name);
    if !known {
        tracing::warn!(
            composed = call.name,
            "compose-only named a tool not in this node's catalog"
        );
    }
    known
}

fn composed_call(output: &Value) -> Option<ToolCall> {
    let written: LlmOutput = serde_json::from_value(output.clone()).ok()?;
    serde_json::from_value(written.tool_call).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const LLM_ROUTER_DECIDE_REQUEST_TIMEOUT_MIRROR: Duration = Duration::from_millis(1500);
    const LLM_ROUTER_COMPLETION_REQUEST_TIMEOUT_MIRROR: Duration = Duration::from_secs(60);

    #[test]
    fn the_engines_outer_decide_deadline_stays_strictly_above_llm_routers_inner_one() {
        assert!(
            DECIDE_CALL_TIMEOUT > LLM_ROUTER_DECIDE_REQUEST_TIMEOUT_MIRROR,
            "this node's outer Decide deadline is asserted as the grpc-timeout llm-router's \
             server starts before its own adapter's inner one, so equal or shorter and the outer \
             always wins the race, dropping llm-router's decision future and its audit write: \
             {DECIDE_CALL_TIMEOUT:?} vs {LLM_ROUTER_DECIDE_REQUEST_TIMEOUT_MIRROR:?}"
        );
    }

    #[test]
    fn the_engines_outer_completion_deadline_stays_strictly_above_llm_routers_inner_one() {
        assert!(
            CALL_TIMEOUT > LLM_ROUTER_COMPLETION_REQUEST_TIMEOUT_MIRROR,
            "the same race as the Decide pair above, and the same cost: equal deadlines mean the \
             outer always wins, so llm-router's audit row is dropped for exactly the slow \
             completion worth investigating: {CALL_TIMEOUT:?} vs \
             {LLM_ROUTER_COMPLETION_REQUEST_TIMEOUT_MIRROR:?}"
        );
    }

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
        let state = json!({"question": "q", "tool_result": ["x".repeat(MAX_CHARS_PER_RENDERED_TOOL_RESULT * 2)]});

        let (_, user) = build_prompt(&config_of(json!({})), &state);

        assert!(user.contains(TRUNCATION_MARK), "{user}");
        assert!(
            user.chars().count() < MAX_CHARS_PER_RENDERED_TOOL_RESULT * 2,
            "{user}"
        );
    }

    #[test]
    fn build_prompt_carries_two_attached_pages_through_the_rendering_cap() {
        let first_page = "alpha ".repeat(900);
        let second_page = "beta ".repeat(900);
        let state = json!({
            "question": "q",
            "tool_result": [[
                {"title": "First", "url": "https://a.example", "snippet": "s", "text": first_page},
                {"title": "Second", "url": "https://b.example", "snippet": "s", "text": second_page},
            ]],
        });

        let (_, user) = build_prompt(&config_of(json!({})), &state);

        assert!(!user.contains(TRUNCATION_MARK), "{user}");
        assert!(
            user.contains(first_page.trim()),
            "the first page's text must survive whole"
        );
        assert!(
            user.contains(second_page.trim()),
            "the second page's text must survive whole"
        );
    }

    #[test]
    fn the_tool_use_policy_names_no_subject_and_no_tool() {
        let lowered = format!(
            "{TOOL_USE_POLICY}{FINAL_ANSWER_STYLE}{TOOL_QUESTION_INSTRUCTIONS}\
             {NO_TOOL_DESCRIPTION}{DECLINE_OPENING}{DECLINE_CLOSING}{NOTHING_CONNECTED}\
             {COMPOSE_REFUSAL}{COMPOSE_FAILURE}{}\
             {PROMISE_INSTRUCTIONS}{PROMISE_WHEN_TRUE}{PROMISE_WHEN_FALSE}{PROMISE_REFUSED}",
            *COMPOSE_ONLY_INSTRUCTIONS,
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
    fn parse_llm_output_recovers_an_envelope_written_after_a_sentence() {
        let output = parse_llm_output(
            r#"Bishkek. {"tool_call": {"name": "weather_now", "args": {"place": "Bishkek"}}, "reply": null}"#,
        );
        assert_eq!(output["tool_call"]["name"], json!("weather_now"));
    }

    #[test]
    fn parse_llm_output_takes_the_first_of_two_envelopes() {
        let output = parse_llm_output(
            r#"{"tool_call": {"name": "weather_now", "args": {}}, "reply": null}
{"tool_call": {"name": "currency_rate", "args": {}}, "reply": null}"#,
        );
        assert_eq!(output["tool_call"]["name"], json!("weather_now"));
    }

    #[test]
    fn parse_llm_output_leaves_prose_containing_a_brace_alone() {
        let content = "Write {name} where the placeholder is.";
        let output = parse_llm_output(content);
        assert_eq!(output["reply"], json!(content));
        assert_eq!(output["tool_call"], Value::Null);
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

    const CONFIDENTLY_FALSE: f64 = 0.02;
    const FAKE_TOKENS_IN: i32 = 17;
    const FAKE_TOKENS_OUT: i32 = 23;
    const FAKE_DECIDE_TOKENS_IN: i32 = 5;
    const FAKE_DECIDE_TOKENS_OUT: i32 = 2;
    const UNREACHABLE_TOOL_SERVICE: &str = "http://127.0.0.1:1";

    struct FakeLlmRouter {
        received: Mutex<Vec<CompleteRequest>>,
        reply: String,
        then_reply: Option<String>,
        decide_tool: Mutex<Option<(String, f64)>>,
        decide_span: Mutex<Option<(String, f64, f64, f64)>>,
        decide_needs_external_info: Mutex<Option<f64>>,
        decide_promise: Mutex<Option<f64>>,
        decide_grounded: Mutex<Vec<f64>>,
        decide_pointed_at: Mutex<Option<f64>>,
        grounded_checks: Mutex<usize>,
        decide_hangs: Mutex<Option<Arc<tokio::sync::Barrier>>>,
    }

    impl FakeLlmRouter {
        fn always(reply: &str) -> Self {
            Self {
                received: Mutex::new(Vec::new()),
                reply: reply.to_owned(),
                then_reply: None,
                decide_tool: Mutex::new(None),
                decide_span: Mutex::new(None),
                decide_needs_external_info: Mutex::new(None),
                decide_promise: Mutex::new(None),
                decide_grounded: Mutex::new(Vec::new()),
                decide_pointed_at: Mutex::new(None),
                grounded_checks: Mutex::new(0),
                decide_hangs: Mutex::new(None),
            }
        }

        fn then(reply: &str, then_reply: &str) -> Self {
            Self {
                received: Mutex::new(Vec::new()),
                reply: reply.to_owned(),
                then_reply: Some(then_reply.to_owned()),
                decide_tool: Mutex::new(None),
                decide_span: Mutex::new(None),
                decide_needs_external_info: Mutex::new(None),
                decide_promise: Mutex::new(None),
                decide_grounded: Mutex::new(Vec::new()),
                decide_pointed_at: Mutex::new(None),
                grounded_checks: Mutex::new(0),
                decide_hangs: Mutex::new(None),
            }
        }

        fn calls(&self) -> usize {
            self.received.lock().expect("lock").len()
        }

        fn system_prompt(&self, nth: usize) -> String {
            self.received.lock().expect("lock")[nth]
                .system_prompt
                .clone()
        }

        fn answer_decide_span(&self, choice: &str, confidence: f64, stated: f64, one_only: f64) {
            *self.decide_span.lock().expect("lock") =
                Some((choice.to_owned(), confidence, stated, one_only));
        }

        fn answer_decide_tool(&self, choice: &str, confidence: f64) {
            *self.decide_tool.lock().expect("lock") = Some((choice.to_owned(), confidence));
        }

        fn answer_decide_needs_external_info(&self, noul: f64) {
            *self.decide_needs_external_info.lock().expect("lock") = Some(noul);
        }

        fn answer_decide_promise(&self, noul: f64) {
            *self.decide_promise.lock().expect("lock") = Some(noul);
        }

        fn answer_decide_pointed_at(&self, noul: f64) {
            *self.decide_pointed_at.lock().expect("lock") = Some(noul);
        }

        fn answer_decide_grounded(&self, one_verdict_per_check: &[f64]) {
            *self.decide_grounded.lock().expect("lock") = one_verdict_per_check.to_vec();
        }

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
            let barrier = self.decide_hangs.lock().expect("lock").clone();
            if let Some(barrier) = barrier {
                barrier.wait().await;
            }
            let tool = self.decide_tool.lock().expect("lock").clone();
            let span = self.decide_span.lock().expect("lock").clone();
            let promise = *self.decide_promise.lock().expect("lock");
            let grounded = self.decide_grounded.lock().expect("lock").clone();
            let pointed_at = *self.decide_pointed_at.lock().expect("lock");
            if tool.is_none() && promise.is_none() && grounded.is_empty() && pointed_at.is_none() {
                return Err(connectrpc::ConnectError::unavailable(
                    "configured fake decider failure",
                ));
            }
            let owned = request.to_owned_message();
            let answers = owned
                .questions
                .iter()
                .filter_map(|question| {
                    if question.id == POINTED_AT_ID {
                        return pointed_at.map(|noul| noul_answer(POINTED_AT_ID, noul));
                    }
                    if question.id == PROMISE_ID {
                        return promise.map(|noul| noul_answer(PROMISE_ID, noul));
                    }
                    if question.id == GROUNDED_ID {
                        if grounded.is_empty() {
                            return None;
                        }
                        let mut checks = self.grounded_checks.lock().expect("lock");
                        let noul = grounded[(*checks).min(grounded.len() - 1)];
                        *checks += 1;
                        return Some(noul_answer(GROUNDED_ID, noul));
                    }
                    scripted_answer(&question.id, &tool, &span)
                })
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

    fn noul_answer(id: &str, noul: f64) -> Answer {
        Answer {
            id: id.to_owned(),
            answer: NoulAnswer {
                noul,
                ..Default::default()
            }
            .into(),
            ..Default::default()
        }
    }

    fn scripted_answer(
        id: &str,
        tool: &Option<(String, f64)>,
        span: &Option<(String, f64, f64, f64)>,
    ) -> Option<Answer> {
        if let Some((choice, confidence, stated, one_only)) = span {
            if let Some(noul) = id
                .starts_with(VALUE_STATED_PREFIX)
                .then_some(*stated)
                .or_else(|| id.starts_with(VALUE_ONE_ONLY_PREFIX).then_some(*one_only))
            {
                return Some(Answer {
                    id: id.to_owned(),
                    answer: NoulAnswer {
                        noul,
                        ..Default::default()
                    }
                    .into(),
                    ..Default::default()
                });
            }
            if id.starts_with(VALUE_QUESTION_PREFIX) {
                return Some(Answer {
                    id: id.to_owned(),
                    answer: ChoiceAnswer {
                        choice: choice.clone(),
                        confidence: *confidence,
                        ..Default::default()
                    }
                    .into(),
                    ..Default::default()
                });
            }
        }
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
            _ => None,
        }
    }

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

        fn first_args(&self) -> Value {
            let received = self.received.lock().expect("lock");
            let first = received.first().expect("a dispatch");
            serde_json::from_str(&first.input_json).expect("argument json")
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

    fn free_text_field_tool(name: &str, field: &str) -> CatalogEntry {
        CatalogEntry {
            name: name.to_owned(),
            title: name.to_owned(),
            description: "does a thing".to_owned(),
            input_schema_json: json!({
                "type": "object",
                "required": [field],
                "properties": {field: {"type": "string", "x-accepts-free-text": true}},
            })
            .to_string(),
        }
    }

    fn value_field_tool(name: &str, field: &str) -> CatalogEntry {
        CatalogEntry {
            name: name.to_owned(),
            title: name.to_owned(),
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
            title: name.to_owned(),
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
    async fn execute_sends_the_built_prompt_and_parses_a_tool_call_reply_once_a_source_has_run() {
        let fake = Arc::new(FakeLlmRouter::always(
            r#"{"tool_call": {"name": "web_search", "args": {"query": "sf weather"}}}"#,
        ));
        let url = serve(Arc::clone(&fake)).await;
        let executor = LlmTaskExecutor::new(&url, UNREACHABLE_TOOL_SERVICE).expect("client");

        let output = executor
            .execute(
                "llm",
                &json!({"tool_calling": true}),
                &json!({
                    "question": "what is it?",
                    TOOL_RESULT_STATE_KEY: [{"observed": "something"}],
                }),
                "e:llm:0",
            )
            .await
            .expect("execute");

        assert_eq!(output["tool_call"]["name"], json!("web_search"));
        let received = fake.received.lock().expect("lock");
        assert_eq!(received.len(), 1);
        assert_eq!(received[0].tier, EnumValue::Known(QualityTier::Medium));
        assert!(received[0].system_prompt.contains("tool_call"));
        assert!(
            received[0].user_prompt.starts_with("what is it?"),
            "the question leads, with the result that has come back rendered after it"
        );
        assert_eq!(
            received[0].sampling.response_format,
            Some(EnumValue::Known(ResponseFormat::JsonObject))
        );
    }

    #[tokio::test]
    async fn an_answer_that_follows_a_tool_result_is_never_nudged() {
        let fake = Arc::new(FakeLlmRouter::always(
            r#"{"tool_call": null, "reply": "62F and foggy."}"#,
        ));
        let url = serve(Arc::clone(&fake)).await;
        let executor = LlmTaskExecutor::new(&url, UNREACHABLE_TOOL_SERVICE).expect("client");

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
        let executor = LlmTaskExecutor::new(&url, UNREACHABLE_TOOL_SERVICE).expect("client");

        executor
            .execute("llm", &json!({}), &json!({"question": "2+2?"}), "e:llm:0")
            .await
            .expect("execute");

        assert_eq!(fake.calls(), 1);
    }

    #[tokio::test]
    async fn execute_reports_the_routers_token_usage_for_the_budget_to_charge() {
        let fake = Arc::new(FakeLlmRouter::always("Four."));
        let url = serve(Arc::clone(&fake)).await;
        let executor = LlmTaskExecutor::new(&url, UNREACHABLE_TOOL_SERVICE).expect("client");

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
        let executor = LlmTaskExecutor::new(&url, UNREACHABLE_TOOL_SERVICE).expect("client");

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
    fn single_required_free_text_field_accepts_exactly_one_required_string() {
        let schema = json!({
            "type": "object",
            "required": ["query"],
            "properties": {"query": {"type": "string", "x-accepts-free-text": true}},
        })
        .to_string();
        assert_eq!(
            single_required_free_text_field(&schema),
            Some("query".to_owned())
        );
    }

    #[test]
    fn one_required_string_of_a_live_catalog_entry_beside_an_optional_sibling_is_accepted() {
        let schema = json!({
            "type": "object",
            "required": ["query"],
            "properties": {
                "query": {"type": "string", "x-accepts-free-text": true},
                "limit": {"type": ["integer", "null"]},
            },
        })
        .to_string();
        assert_eq!(
            single_required_free_text_field(&schema),
            Some("query".to_owned())
        );
    }

    #[test]
    fn single_required_free_text_field_rejects_two_required_fields() {
        let schema = json!({
            "type": "object",
            "required": ["a", "b"],
            "properties": {"a": {"type": "string"}, "b": {"type": "string"}},
        })
        .to_string();
        assert_eq!(single_required_free_text_field(&schema), None);
    }

    fn span_field_tool(name: &str, field: &str) -> CatalogEntry {
        CatalogEntry {
            name: name.to_owned(),
            title: name.to_owned(),
            description: "does a thing".to_owned(),
            input_schema_json: json!({
                "type": "object",
                "required": [field],
                "properties": {field: {
                    "type": "string",
                    "description": "the place whose reading is asked about",
                    "x-value-in-message": true,
                }},
            })
            .to_string(),
        }
    }

    #[test]
    fn a_span_equal_to_the_no_value_sentinel_is_never_offered() {
        let offered = spans("none of those, what about Bishkek?");

        assert!(
            !offered.iter().any(|span| span == NO_VALUE_OPTION),
            "the sentinel and the message's own words share one option namespace, and a \
             duplicate name is refused by the decider — taking the whole turn down: {offered:?}"
        );
        assert!(offered.iter().any(|span| span == "Bishkek"));
    }

    #[test]
    fn a_span_longer_than_an_option_name_may_be_is_dropped() {
        let one_long_word = "x".repeat(MAX_OPTION_NAME_BYTES + 1);

        let offered = spans(&format!("{one_long_word} and Bishkek"));

        assert!(
            offered
                .iter()
                .all(|span| span.len() <= MAX_OPTION_NAME_BYTES),
            "an over-long option is refused by the decider, and a script that whitespace does \
             not segment yields one span per clause: {offered:?}"
        );
    }

    #[test]
    fn spans_stop_at_the_number_of_options_the_decider_accepts() {
        let many_words = (0..500)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(" ");

        assert!(spans(&many_words).len() <= MAX_SPAN_OPTIONS);
    }

    #[test]
    fn spans_offers_each_word_and_the_runs_of_words_beside_it() {
        assert_eq!(
            spans("weather in Bishkek?"),
            vec![
                "weather",
                "in",
                "Bishkek",
                "weather in",
                "in Bishkek",
                "weather in Bishkek",
            ]
        );
    }

    #[test]
    fn a_multi_word_value_is_itself_an_option() {
        assert!(spans("weather in New York").contains(&"New York".to_owned()));
    }

    #[test]
    fn spans_does_not_offer_the_same_words_twice() {
        let found = spans("rain rain go away");
        let mut once = found.clone();
        once.sort();
        once.dedup();
        assert_eq!(once.len(), found.len(), "{found:?}");
    }

    #[test]
    fn a_field_that_does_not_say_its_value_is_in_the_message_is_not_asked_about() {
        let plain = json!({
            "type": "object",
            "required": ["place"],
            "properties": {"place": {"type": "string"}},
        })
        .to_string();
        assert_eq!(decidable_argument(&plain), None);
    }

    #[test]
    fn the_questions_carry_the_fields_own_description_rather_than_its_name() {
        let questions = decidable_questions(
            "weather in Bishkek?",
            &[span_field_tool("some_tool", "place")],
        );
        assert_eq!(questions.len(), 3, "one pick and two absolute gates");
        let rendered = serde_json::to_string(&questions).expect("serialize");
        assert!(
            rendered.contains("the place whose reading is asked about"),
            "{rendered}"
        );
        assert!(
            !rendered.contains("`place`"),
            "the parameter's name is not the question"
        );
    }

    #[test]
    fn the_pick_offers_a_way_to_say_none_of_these() {
        let questions = decidable_questions(
            "weather in Bishkek?",
            &[span_field_tool("some_tool", "place")],
        );
        let rendered = serde_json::to_string(&questions[0]).expect("serialize");
        assert!(rendered.contains(NO_VALUE_OPTION), "{rendered}");
    }

    fn listed_field_tool(name: &str, field: &str) -> CatalogEntry {
        CatalogEntry {
            name: name.to_owned(),
            title: name.to_owned(),
            description: "does a thing".to_owned(),
            input_schema_json: json!({
                "type": "object",
                "required": [field],
                "properties": {field: {
                    "type": "string",
                    "description": "the thing being priced",
                    "oneOf": [
                        {"const": "^AAA", "description": "the first index"},
                        {"const": "^BBB", "description": "the second index"},
                    ],
                }},
            })
            .to_string(),
        }
    }

    #[test]
    fn a_listed_argument_offers_the_schemas_values_not_the_messages_words() {
        let questions = decidable_questions(
            "how is the first index doing",
            &[listed_field_tool("some_tool", "symbol")],
        );
        let rendered = serde_json::to_string(&questions[0]).expect("serialize");

        assert!(
            rendered.contains("^AAA") && rendered.contains("^BBB"),
            "{rendered}"
        );
        assert!(
            rendered.contains("the first index"),
            "a line per value: {rendered}"
        );
        assert!(
            !rendered.contains("\"how\""),
            "the message's own words are not the options here: {rendered}"
        );
    }

    #[test]
    fn a_bare_enum_is_a_closed_list_as_well() {
        let schema = json!({
            "type": "object",
            "required": ["symbol"],
            "properties": {"symbol": {"type": "string", "enum": ["^AAA", "^BBB"]}},
        })
        .to_string();
        assert!(decidable_argument(&schema).is_some());
    }

    #[test]
    fn a_listed_argument_is_preferred_to_the_messages_words() {
        let schema = json!({
            "type": "object",
            "required": ["symbol"],
            "properties": {"symbol": {
                "type": "string",
                "enum": ["^AAA"],
                "x-value-in-message": true,
            }},
        })
        .to_string();
        let Some((_, _, options)) = decidable_field(&schema) else {
            panic!("decidable");
        };
        assert!(matches!(options, Options::Closed(_)));
    }

    #[test]
    fn a_field_with_neither_a_list_nor_the_annotation_is_not_decided() {
        let schema = json!({
            "type": "object",
            "required": ["place"],
            "properties": {"place": {"type": "string"}},
        })
        .to_string();
        assert_eq!(decidable_argument(&schema), None);
    }

    fn span_answers(choice: &str, confidence: f64, stated: f64, one_only: f64) -> Vec<Answer> {
        vec![
            Answer {
                id: format!("{VALUE_QUESTION_PREFIX}some_tool"),
                answer: ChoiceAnswer {
                    choice: choice.to_owned(),
                    confidence,
                    ..Default::default()
                }
                .into(),
                ..Default::default()
            },
            Answer {
                id: format!("{VALUE_STATED_PREFIX}some_tool"),
                answer: NoulAnswer {
                    noul: stated,
                    ..Default::default()
                }
                .into(),
                ..Default::default()
            },
            Answer {
                id: format!("{VALUE_ONE_ONLY_PREFIX}some_tool"),
                answer: NoulAnswer {
                    noul: one_only,
                    ..Default::default()
                }
                .into(),
                ..Default::default()
            },
        ]
    }

    #[test]
    fn a_confident_pick_that_both_gates_agree_with_is_used() {
        let answers = span_answers("Bishkek", 0.99, 0.99, 0.9);
        assert_eq!(
            picked_value(&answers, "some_tool"),
            Some("Bishkek".to_owned())
        );
    }

    #[test]
    fn each_gate_refuses_a_confident_pick_on_its_own() {
        for (label, answers) in [
            (
                "the message only implies it",
                span_answers("capital of Kyrgyzstan", 0.98, 0.15, 0.82),
            ),
            (
                "the message names two",
                span_answers("Paris and Berlin", 0.95, 0.99, 0.08),
            ),
            (
                "the choice itself is unsure",
                span_answers("York", 0.4, 0.99, 0.9),
            ),
            (
                "nothing in the message fits",
                span_answers(NO_VALUE_OPTION, 0.99, 0.99, 0.9),
            ),
        ] {
            assert_eq!(picked_value(&answers, "some_tool"), None, "{label}");
        }
    }

    #[test]
    fn single_required_free_text_field_rejects_a_string_that_does_not_take_free_text() {
        let schema = json!({
            "type": "object",
            "required": ["place"],
            "properties": {"place": {"type": "string"}},
        })
        .to_string();
        assert_eq!(single_required_free_text_field(&schema), None);
    }

    #[test]
    fn single_required_free_text_field_rejects_free_text_declared_as_anything_but_true() {
        let schema = json!({
            "type": "object",
            "required": ["place"],
            "properties": {"place": {"type": "string", "x-accepts-free-text": "yes"}},
        })
        .to_string();
        assert_eq!(single_required_free_text_field(&schema), None);
    }

    #[test]
    fn single_required_free_text_field_rejects_a_non_string_required_field() {
        let schema = json!({
            "type": "object",
            "required": ["count"],
            "properties": {"count": {"type": "number"}},
        })
        .to_string();
        assert_eq!(single_required_free_text_field(&schema), None);
    }

    #[test]
    fn single_required_free_text_field_rejects_a_schema_it_cannot_read() {
        assert_eq!(single_required_free_text_field("not json"), None);
        assert_eq!(single_required_free_text_field("{}"), None);
        assert_eq!(single_required_free_text_field(r#"{"required": []}"#), None);
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
            available_tools: vec![free_text_field_tool("some_tool", "query")],
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
    async fn a_span_valued_argument_is_dispatched_with_the_picked_word_not_the_question() {
        let fake = Arc::new(FakeLlmRouter::always(
            r#"{"tool_call": null, "reply": "clear, 15C"}"#,
        ));
        fake.answer_decide_tool("some_tool", 0.9);
        fake.answer_decide_span("Bishkek", 0.99, 0.99, 0.9);
        let llm_url = serve(Arc::clone(&fake)).await;
        let tool = Arc::new(FakeToolService::ok(r#"{"temp": 15}"#));
        let tool_url = serve_tool(Arc::clone(&tool)).await;
        let executor = LlmTaskExecutor::new(&llm_url, &tool_url).expect("client");
        let config = LlmNodeConfig {
            tool_calling: true,
            available_tools: vec![span_field_tool("some_tool", "place")],
            ..Default::default()
        }
        .to_json();

        let output = executor
            .execute(
                "llm",
                &config,
                &json!({"question": "weather in Bishkek?"}),
                "e:llm:0",
            )
            .await
            .expect("execute");

        assert_eq!(
            tool.calls(),
            1,
            "the tool is reached without composing first"
        );
        let calls = tool.received.lock().expect("lock");
        assert_eq!(calls[0].input_json, r#"{"place":"Bishkek"}"#);
        drop(calls);
        assert_eq!(
            fake.calls(),
            1,
            "one Complete for the whole turn: the answer, and nothing to build the call"
        );
        assert_eq!(output["reply"], json!("clear, 15C"));
    }

    #[tokio::test]
    async fn a_span_the_gates_refuse_leaves_the_argument_to_be_composed() {
        let fake = Arc::new(FakeLlmRouter::always(
            r#"{"tool_call": null, "reply": "clear, 15C"}"#,
        ));
        fake.answer_decide_tool("some_tool", 0.9);
        fake.answer_decide_span("capital of Kyrgyzstan", 0.98, 0.15, 0.82);
        let llm_url = serve(Arc::clone(&fake)).await;
        let tool = Arc::new(FakeToolService::ok(r#"{"temp": 15}"#));
        let tool_url = serve_tool(Arc::clone(&tool)).await;
        let executor = LlmTaskExecutor::new(&llm_url, &tool_url).expect("client");
        let config = LlmNodeConfig {
            tool_calling: true,
            available_tools: vec![span_field_tool("some_tool", "place")],
            ..Default::default()
        }
        .to_json();

        let output = executor
            .execute(
                "llm",
                &config,
                &json!({"question": "what is the weather in the capital of Kyrgyzstan?"}),
                "e:llm:0",
            )
            .await
            .expect("execute");

        assert_eq!(tool.calls(), 0, "a refused pick reaches no tool");
        assert!(output.get("fast_tool_call").is_none(), "{output}");
    }

    #[tokio::test]
    async fn a_tool_whose_argument_is_a_value_is_not_dispatched_with_the_question_verbatim() {
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
            available_tools: vec![value_field_tool("some_tool", "place")],
            ..Default::default()
        }
        .to_json();

        let output = executor
            .execute(
                "llm",
                &config,
                &json!({"question": "what is the weather in Bishkek today?"}),
                "e:llm:0",
            )
            .await
            .expect("execute");

        assert_eq!(
            tool.calls(),
            0,
            "no tool may be reached before the model has composed its arguments"
        );
        assert!(
            output.get("fast_tool_call").is_none(),
            "nothing was fast-dispatched: {output}"
        );
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
            json!(i64::from(FAKE_TOKENS_IN + FAKE_DECIDE_TOKENS_IN * 2)),
            "every Decide this path spends is charged: the one that chose the source, and the \
             one that checked the reply it produced"
        );
    }

    #[tokio::test]
    async fn material_older_than_it_may_be_is_declined_rather_than_answered_from() {
        let fake = Arc::new(FakeLlmRouter::always(
            r#"{"tool_call": null, "reply": "29.6"}"#,
        ));
        fake.answer_decide_tool(PRIOR_OPTION, 0.95);
        let llm_url = serve(Arc::clone(&fake)).await;
        let executor = LlmTaskExecutor::new(&llm_url, UNREACHABLE_TOOL_SERVICE).expect("client");
        let config = LlmNodeConfig {
            tool_calling: true,
            available_tools: vec![free_text_field_tool("a_source", "query")],
            ..Default::default()
        }
        .to_json();
        let long_ago = (Utc::now() - CARRIED_MATERIAL_GOES_STALE_AFTER * 2).to_rfc3339();
        let state = json!({
            "question": "and the humidity?",
            PRIOR_MATERIAL_STATE_KEY: [{
                "name": "a_source",
                "args": {"query": "q"},
                "result": {"temp": 29.6},
                FETCHED_AT_FIELD: long_ago,
            }],
        });

        let output = executor
            .execute("llm", &config, &state, "e:llm:0")
            .await
            .expect("execute");

        assert_ne!(
            output["reply"],
            json!("29.6"),
            "a price or a temperature an hour old is still present in the material, so the \
             grounding guard approves it — freshness is the one thing no later check can see"
        );
        assert_eq!(fake.calls(), 0, "and nothing is spent answering from it");
    }

    #[tokio::test]
    async fn a_question_no_connected_source_covers_is_declined_without_a_generative_call() {
        let fake = Arc::new(FakeLlmRouter::always(
            r#"{"tool_call": null, "reply": "Bishkek."}"#,
        ));
        fake.answer_decide_tool(NO_TOOL_OPTION, 0.97);
        let llm_url = serve(Arc::clone(&fake)).await;
        let tool = Arc::new(FakeToolService::ok("{}"));
        let tool_url = serve_tool(Arc::clone(&tool)).await;
        let executor = LlmTaskExecutor::new(&llm_url, &tool_url).expect("client");
        let mut entry = free_text_field_tool("some_slug", "query");
        entry.title = "What the operator calls it".to_owned();
        let config = LlmNodeConfig {
            tool_calling: true,
            available_tools: vec![entry],
            ..Default::default()
        }
        .to_json();

        let output = executor
            .execute("llm", &config, &json!({"question": "q"}), "e:llm:0")
            .await
            .expect("execute");

        assert_eq!(
            fake.calls(),
            0,
            "a decline costs no generative call: the model is never asked, so it cannot answer \
             from memory inside its own refusal"
        );
        assert_eq!(tool.calls(), 0);
        let reply = output["reply"].as_str().expect("a reply");
        assert!(
            reply.contains("What the operator calls it"),
            "the decline names sources the way the operator named them, not by slug: {reply:?}"
        );
        assert!(
            reply.contains("did you mean"),
            "and asks which of it was meant, so an adjacent question has a way back in: {reply:?}"
        );
    }

    #[tokio::test]
    async fn the_decline_names_one_source_once_however_many_tools_reach_it() {
        let fake = Arc::new(FakeLlmRouter::always(
            r#"{"tool_call": null, "reply": "x"}"#,
        ));
        fake.answer_decide_tool(NO_TOOL_OPTION, 0.97);
        let llm_url = serve(Arc::clone(&fake)).await;
        let executor = LlmTaskExecutor::new(&llm_url, UNREACHABLE_TOOL_SERVICE).expect("client");
        let mut searching = free_text_field_tool("search_it", "query");
        searching.title = "The stored material".to_owned();
        let mut reading = free_text_field_tool("read_it", "query");
        reading.title = "The stored material".to_owned();
        let config = LlmNodeConfig {
            tool_calling: true,
            available_tools: vec![searching, reading],
            ..Default::default()
        }
        .to_json();

        let output = executor
            .execute("llm", &config, &json!({"question": "q"}), "e:llm:0")
            .await
            .expect("execute");

        let reply = output["reply"].as_str().expect("a reply");
        assert_eq!(
            reply.matches("The stored material").count(),
            1,
            "naming the same source twice reads as a fault: {reply:?}"
        );
    }

    #[tokio::test]
    async fn a_decision_that_never_arrives_declines_rather_than_answering_from_memory() {
        let fake = Arc::new(FakeLlmRouter::always(
            r#"{"tool_call": null, "reply": "Bishkek."}"#,
        ));
        let llm_url = serve(Arc::clone(&fake)).await;
        let tool = Arc::new(FakeToolService::ok("{}"));
        let tool_url = serve_tool(Arc::clone(&tool)).await;
        let executor = LlmTaskExecutor::new(&llm_url, &tool_url).expect("client");
        let config = LlmNodeConfig {
            tool_calling: true,
            available_tools: vec![free_text_field_tool("Current weather for a place", "query")],
            ..Default::default()
        }
        .to_json();

        let output = executor
            .execute("llm", &config, &json!({"question": "q"}), "e:llm:0")
            .await
            .expect("execute");

        assert_eq!(
            fake.calls(),
            0,
            "no generative call is made on this path either"
        );
        assert_ne!(output["reply"], json!("Bishkek."));
        assert!(
            output["reply"]
                .as_str()
                .is_some_and(|reply| reply.contains("Current weather for a place")),
            "an outage still tells the person what this is for"
        );
    }

    #[tokio::test]
    async fn a_pick_naming_a_tool_the_catalog_does_not_carry_declines() {
        let fake = Arc::new(FakeLlmRouter::always(
            r#"{"tool_call": null, "reply": "Bishkek."}"#,
        ));
        fake.answer_decide_tool("a_tool_that_left", 0.95);
        let llm_url = serve(Arc::clone(&fake)).await;
        let tool = Arc::new(FakeToolService::ok("{}"));
        let tool_url = serve_tool(Arc::clone(&tool)).await;
        let executor = LlmTaskExecutor::new(&llm_url, &tool_url).expect("client");
        let config = LlmNodeConfig {
            tool_calling: true,
            available_tools: vec![free_text_field_tool("Current weather for a place", "query")],
            ..Default::default()
        }
        .to_json();

        let output = executor
            .execute("llm", &config, &json!({"question": "q"}), "e:llm:0")
            .await
            .expect("execute");

        assert_eq!(
            tool.calls(),
            0,
            "a tool that is not in the catalog is never dispatched"
        );
        assert_ne!(output["reply"], json!("Bishkek."));
    }

    #[tokio::test]
    async fn a_pick_too_weak_to_act_on_declines() {
        let fake = Arc::new(FakeLlmRouter::always(
            r#"{"tool_call": null, "reply": "Bishkek."}"#,
        ));
        fake.answer_decide_tool("Current weather for a place", 0.2);
        let llm_url = serve(Arc::clone(&fake)).await;
        let tool = Arc::new(FakeToolService::ok("{}"));
        let tool_url = serve_tool(Arc::clone(&tool)).await;
        let executor = LlmTaskExecutor::new(&llm_url, &tool_url).expect("client");
        let config = LlmNodeConfig {
            tool_calling: true,
            available_tools: vec![free_text_field_tool("Current weather for a place", "query")],
            ..Default::default()
        }
        .to_json();

        let output = executor
            .execute("llm", &config, &json!({"question": "q"}), "e:llm:0")
            .await
            .expect("execute");

        assert_eq!(fake.calls(), 0);
        assert_ne!(output["reply"], json!("Bishkek."));
    }

    #[tokio::test]
    async fn an_empty_catalog_says_so_rather_than_listing_nothing() {
        let fake = Arc::new(FakeLlmRouter::always(
            r#"{"tool_call": null, "reply": "x"}"#,
        ));
        let llm_url = serve(Arc::clone(&fake)).await;
        let executor = LlmTaskExecutor::new(&llm_url, UNREACHABLE_TOOL_SERVICE).expect("client");
        let config = LlmNodeConfig {
            tool_calling: true,
            ..Default::default()
        }
        .to_json();

        let output = executor
            .execute("llm", &config, &json!({"question": "q"}), "e:llm:0")
            .await
            .expect("execute");

        assert_eq!(output["reply"], json!(NOTHING_CONNECTED));
    }

    #[tokio::test]
    async fn a_value_the_message_points_at_nothing_for_is_asked_about_rather_than_dispatched() {
        let fake = Arc::new(FakeLlmRouter::always(
            r#"{"tool_call": {"name": "weather", "args": {"place": "there"}}, "reply": null}"#,
        ));
        fake.answer_decide_tool("weather", 0.95);
        fake.answer_decide_pointed_at(0.62);
        let llm_url = serve(Arc::clone(&fake)).await;
        let tool = Arc::new(FakeToolService::ok(r#"{"temp": 25.9}"#));
        let tool_url = serve_tool(Arc::clone(&tool)).await;
        let executor = LlmTaskExecutor::new(&llm_url, &tool_url).expect("client");
        let mut entry = free_text_field_tool("weather", "place");
        entry.input_schema_json = json!({
            "type": "object",
            "required": ["place"],
            "properties": {"place": {
                "type": "string",
                "description": "place do you mean",
                "x-value-in-message": true,
            }},
        })
        .to_string();
        let config = LlmNodeConfig {
            tool_calling: true,
            available_tools: vec![entry],
            ..Default::default()
        }
        .to_json();

        let output = executor
            .execute(
                "llm",
                &config,
                &json!({"question": "weather there?"}),
                "e:llm:0",
            )
            .await
            .expect("execute");

        assert_eq!(
            tool.calls(),
            0,
            "a value the message does not point at must never reach a source: it answers for \
             whatever it is asked, and the wrong answer then looks exactly like a right one"
        );
        let reply = output["reply"].as_str().expect("a reply");
        assert!(
            reply.contains("place do you mean"),
            "the question back is worded from the schema the operator wrote: {reply:?}"
        );
    }

    #[tokio::test]
    async fn a_value_the_message_describes_unambiguously_is_dispatched() {
        let fake = Arc::new(FakeLlmRouter::always(
            r#"{"tool_call": {"name": "weather", "args": {"place": "Tokyo"}}, "reply": null}"#,
        ));
        fake.answer_decide_tool("weather", 0.95);
        fake.answer_decide_pointed_at(0.97);
        fake.answer_decide_grounded(&[0.95]);
        let llm_url = serve(Arc::clone(&fake)).await;
        let tool = Arc::new(FakeToolService::ok(r#"{"temp": 29.6}"#));
        let tool_url = serve_tool(Arc::clone(&tool)).await;
        let executor = LlmTaskExecutor::new(&llm_url, &tool_url).expect("client");
        let config = LlmNodeConfig {
            tool_calling: true,
            available_tools: vec![free_text_field_tool("weather", "place")],
            ..Default::default()
        }
        .to_json();

        executor
            .execute(
                "llm",
                &config,
                &json!({"question": "weather in the capital of Japan?"}),
                "e:llm:0",
            )
            .await
            .expect("execute");

        assert_eq!(
            tool.calls(),
            1,
            "the message points at one place, so the call goes"
        );
    }

    #[tokio::test]
    async fn a_reply_stating_what_the_material_does_not_hold_is_asked_again() {
        let fake = Arc::new(FakeLlmRouter::then(
            r#"{"tool_call": null, "reply": "Bishkek"}"#,
            r#"{"tool_call": null, "reply": "22.11"}"#,
        ));
        fake.answer_decide_tool("weather", 0.95);
        fake.answer_decide_grounded(&[0.05, 0.98]);
        let llm_url = serve(Arc::clone(&fake)).await;
        let tool = Arc::new(FakeToolService::ok(r#"{"temp": 22.11}"#));
        let tool_url = serve_tool(Arc::clone(&tool)).await;
        let executor = LlmTaskExecutor::new(&llm_url, &tool_url).expect("client");
        let config = LlmNodeConfig {
            tool_calling: true,
            available_tools: vec![free_text_field_tool("weather", "query")],
            ..Default::default()
        }
        .to_json();

        let output = executor
            .execute("llm", &config, &json!({"question": "q"}), "e:llm:0")
            .await
            .expect("execute");

        assert_eq!(output["reply"], json!("22.11"));
        let asked_again = fake.system_prompt(1);
        assert!(
            asked_again.contains("\"Bishkek\""),
            "the second ask quotes the words it refused, or the model writes them again: \
             {asked_again}"
        );
    }

    #[tokio::test]
    async fn a_reply_that_stays_ungrounded_is_refused_rather_than_sent() {
        let fake = Arc::new(FakeLlmRouter::always(
            r#"{"tool_call": null, "reply": "31.4 degrees"}"#,
        ));
        fake.answer_decide_tool("weather", 0.95);
        fake.answer_decide_grounded(&[0.03]);
        let llm_url = serve(Arc::clone(&fake)).await;
        let tool = Arc::new(FakeToolService::ok(r#"{"temp": 22.11}"#));
        let tool_url = serve_tool(Arc::clone(&tool)).await;
        let executor = LlmTaskExecutor::new(&llm_url, &tool_url).expect("client");
        let config = LlmNodeConfig {
            tool_calling: true,
            available_tools: vec![free_text_field_tool("weather", "query")],
            ..Default::default()
        }
        .to_json();

        let output = executor
            .execute("llm", &config, &json!({"question": "q"}), "e:llm:0")
            .await
            .expect("execute");

        assert_eq!(output["reply"], json!(UNGROUNDED_REFUSED));
    }

    #[tokio::test]
    async fn a_grounded_answer_passes_untouched() {
        let fake = Arc::new(FakeLlmRouter::always(
            r#"{"tool_call": null, "reply": "22.11"}"#,
        ));
        fake.answer_decide_tool("weather", 0.95);
        fake.answer_decide_grounded(&[0.98]);
        fake.answer_decide_promise(0.02);
        let llm_url = serve(Arc::clone(&fake)).await;
        let tool = Arc::new(FakeToolService::ok(r#"{"temp": 22.11}"#));
        let tool_url = serve_tool(Arc::clone(&tool)).await;
        let executor = LlmTaskExecutor::new(&llm_url, &tool_url).expect("client");
        let config = LlmNodeConfig {
            tool_calling: true,
            available_tools: vec![free_text_field_tool("weather", "query")],
            ..Default::default()
        }
        .to_json();

        let output = executor
            .execute("llm", &config, &json!({"question": "q"}), "e:llm:0")
            .await
            .expect("execute");

        assert_eq!(output["reply"], json!("22.11"));
        assert_eq!(fake.calls(), 1, "and costs no second generative call");
    }

    #[tokio::test]
    async fn a_promise_written_after_a_source_ran_is_replaced_with_the_plain_truth() {
        let fake = Arc::new(FakeLlmRouter::always(
            r#"{"tool_call": null, "reply": "I'll go and check that for you."}"#,
        ));
        fake.answer_decide_tool("Current weather for a place", 0.95);
        fake.answer_decide_promise(0.9);
        let llm_url = serve(Arc::clone(&fake)).await;
        let tool = Arc::new(FakeToolService::ok(r#"{"temp": 21}"#));
        let tool_url = serve_tool(Arc::clone(&tool)).await;
        let executor = LlmTaskExecutor::new(&llm_url, &tool_url).expect("client");
        let config = LlmNodeConfig {
            tool_calling: true,
            available_tools: vec![free_text_field_tool("Current weather for a place", "query")],
            ..Default::default()
        }
        .to_json();

        let output = executor
            .execute("llm", &config, &json!({"question": "q"}), "e:llm:0")
            .await
            .expect("execute");

        assert_eq!(tool.calls(), 1, "the source did run");
        assert_eq!(
            output["reply"],
            json!(PROMISE_REFUSED),
            "but the reply undertook work instead of reporting it"
        );
    }

    #[tokio::test]
    async fn a_tool_needing_two_fields_is_composed_rather_than_dispatched_verbatim() {
        let fake = Arc::new(FakeLlmRouter::always(
            r#"{"tool_call": {"name": "two_field_tool", "args": {"a": "x", "b": "y"}}}"#,
        ));
        fake.answer_decide_tool("two_field_tool", 0.95);
        fake.answer_decide_pointed_at(0.95);
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

        assert_eq!(
            tool.calls(),
            1,
            "the tool the decision chose is still called"
        );
        assert_eq!(
            tool.first_args(),
            json!({"a": "x", "b": "y"}),
            "with the arguments the model composed, never the question verbatim"
        );
        assert_eq!(
            output[FAST_TOOL_CALL_FIELD]["name"],
            json!("two_field_tool"),
            "and the call is recorded, so it is event-logged and budget-charged like any other"
        );
        assert!(
            fake.system_prompt(0).contains("not accepted here"),
            "the composing call refuses a plain reply"
        );
    }

    #[tokio::test]
    async fn a_compose_only_reply_without_a_call_is_refused_and_asked_again() {
        let fake = Arc::new(FakeLlmRouter::then(
            r#"{"tool_call": null, "reply": "Sure, I will look that up for you."}"#,
            r#"{"tool_call": {"name": "two_field_tool", "args": {"a": "x", "b": "y"}}}"#,
        ));
        fake.answer_decide_tool("two_field_tool", 0.95);
        fake.answer_decide_pointed_at(0.95);
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
            1,
            "the promise is refused and the second response's call is dispatched"
        );
        assert!(
            fake.system_prompt(1).contains("was refused"),
            "and the second ask states the refusal rather than repeating the instruction"
        );
    }

    #[tokio::test]
    async fn a_compose_only_reply_refused_twice_answers_plainly_instead_of_promising() {
        let fake = Arc::new(FakeLlmRouter::always(
            r#"{"tool_call": null, "reply": "Sure, I will look that up for you."}"#,
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

        let output = executor
            .execute("llm", &config, &json!({"question": "q"}), "e:llm:0")
            .await
            .expect("execute");

        assert_eq!(tool.calls(), 0, "there was never a call to dispatch");
        assert_eq!(output["tool_call"], Value::Null);
        assert_eq!(
            output["reply"],
            json!(COMPOSE_FAILURE),
            "the turn reports the failure; it never forwards the promise"
        );
        assert_eq!(fake.calls(), 2, "asked twice, not a third time");
    }

    #[tokio::test]
    async fn a_confident_schema_disqualified_tool_pick_vetoes_no_tool_needed_too() {
        let fake = Arc::new(FakeLlmRouter::always(
            r#"{"tool_call": {"name": "two_field_tool", "args": {"a": "x", "b": "y"}}}"#,
        ));
        fake.answer_decide_tool("two_field_tool", 0.95);
        fake.answer_decide_needs_external_info(CONFIDENTLY_FALSE);
        fake.answer_decide_pointed_at(0.95);
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

        assert!(
            fake.system_prompt(0).contains("not accepted here"),
            "the confident pick sends the turn to compose the call, never to answer without one"
        );
        assert_eq!(tool.calls(), 1, "and the tool the decision named is called");
    }

    #[tokio::test]
    async fn a_multi_line_question_is_not_fast_dispatched_even_when_the_tool_otherwise_qualifies() {
        let fake = Arc::new(FakeLlmRouter::always(
            r#"{"tool_call": {"name": "some_tool", "args": {"query": "population of Paris"}}}"#,
        ));
        fake.answer_decide_tool("some_tool", 0.9);
        fake.answer_decide_needs_external_info(CONFIDENTLY_FALSE);
        fake.answer_decide_pointed_at(0.95);
        let llm_url = serve(Arc::clone(&fake)).await;
        let tool = Arc::new(FakeToolService::ok("{}"));
        let tool_url = serve_tool(Arc::clone(&tool)).await;
        let executor = LlmTaskExecutor::new(&llm_url, &tool_url).expect("client");
        let config = LlmNodeConfig {
            tool_calling: true,
            available_tools: vec![free_text_field_tool("some_tool", "query")],
            ..Default::default()
        }
        .to_json();
        let question = "Earlier answer:\nParis\n\nFollow-up: what is its population?";

        executor
            .execute("llm", &config, &json!({"question": question}), "e:llm:0")
            .await
            .expect("execute");

        assert_eq!(
            tool.calls(),
            1,
            "the tool the decision chose is still called"
        );
        assert_ne!(
            tool.first_args()["query"],
            json!(question),
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
            available_tools: vec![free_text_field_tool("some_tool", "query")],
            ..Default::default()
        }
        .to_json();
        let question = "x".repeat(MAX_CHARS_SENT_TO_A_TOOL_VERBATIM + 1);

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

    #[tokio::test(start_paused = true)]
    async fn a_hung_decide_call_is_abandoned_within_its_own_deadline() {
        let fake = Arc::new(FakeLlmRouter::always(
            r#"{"tool_call": null, "reply": "ok"}"#,
        ));
        fake.hang_decide(Arc::new(tokio::sync::Barrier::new(2)));
        let url = serve(Arc::clone(&fake)).await;
        let executor = LlmTaskExecutor::new(&url, UNREACHABLE_TOOL_SERVICE).expect("client");
        let config = LlmNodeConfig {
            tool_calling: true,
            ..Default::default()
        };

        let started = tokio::time::Instant::now();
        let outcome = executor
            .decide_fast_path(&config, "which courses exist?", None)
            .await;
        let elapsed = started.elapsed();

        assert!(
            matches!(outcome, FastPath::Decline { .. }),
            "a decider that does not answer must decline, never fall through to answering from \
             memory — the guarantee cannot hold only while the provider is up"
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
    async fn a_later_iteration_is_not_re_scoped_but_its_answer_is_still_checked() {
        let fake = Arc::new(FakeLlmRouter::always(
            r#"{"tool_call": null, "reply": "done"}"#,
        ));
        fake.answer_decide_tool("a_source", 0.99);
        fake.answer_decide_grounded(&[0.95]);
        let url = serve(Arc::clone(&fake)).await;
        let executor = LlmTaskExecutor::new(&url, UNREACHABLE_TOOL_SERVICE).expect("client");

        let output = executor
            .execute(
                "llm",
                &json!({"tool_calling": true}),
                &json!({"question": "q", "tool_result": [{"ok": true}]}),
                "e:llm:0",
            )
            .await
            .expect("execute");

        assert_eq!(
            output["reply"],
            json!("done"),
            "the scope decision does not run again — a turn that has already fetched is past it"
        );
        assert_eq!(
            fake.received.lock().expect("lock").len(),
            1,
            "and it answers in one generative call, not by re-entering the fast path"
        );
    }
}
