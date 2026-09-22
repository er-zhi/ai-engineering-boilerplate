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
/// What the node is being asked for on this call. The typed decision before it has already settled
/// whether a tool is needed; offering the model that choice again is how a light model comes back
/// with "I will look that up" and ends the turn having done nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Both outcomes are open and the model picks. The residual case: the decision reached neither
    /// conclusion confidently.
    Loop,
    /// A source is needed and only its arguments are missing. Tools are offered; a reply is
    /// refused.
    ComposeOnly,
}

static COMPOSE_ONLY_INSTRUCTIONS: LazyLock<String> = LazyLock::new(|| {
    format!(
        r#"
Call one of the tools below. Respond with exactly this JSON and nothing else:
{}
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
const CALL_TIMEOUT: Duration = Duration::from_secs(60);
const RETRY_ATTEMPTS: u32 = 3;
const RETRY_BASE_DELAY: Duration = Duration::from_millis(200);
const RENDERED_TOOL_RESULTS: usize = 4;
/// Applied to one whole rendered tool result (not per hit within it — see below), so a search that
/// attached the text of several pages (Task C, `docs/superpowers/plans/2026-09-18-latency-round-two.md`)
/// is not cut before the model ever sees the pages. Flat-per-result rather than a cap per hit: the
/// engine has no notion of a "hit" — that shape belongs to the tool that produced the JSON, and
/// giving the engine a per-hit cap would mean teaching it that shape, which is exactly the kind of
/// subject-specific knowledge `gate-architecture`'s "Capabilities, Not Topics" forbids landing here.
/// A flat cap needs no such knowledge and is nearly free: input tokens cost about 0.29 ms each in
/// this deployment (r = 0.09 against total latency), so raising the ceiling by 8 000 chars (~2 000
/// tokens) costs under 600 ms in the rare case a result actually reaches it — far less than the
/// generative round this change exists to remove. 12 000 was chosen as comfortably above two
/// attached pages' typical readable text (a few thousand characters each) plus the surrounding
/// title/url/snippet JSON, while still bounding the pathological case (a very long single page).
const MAX_TOOL_RESULT_CHARS: usize = 12_000;
const TRUNCATION_MARK: &str = "… (truncated)";

/// `Decide` gets its own deadline, separate from `CALL_TIMEOUT` (`Complete`'s). Same measurement
/// `services/chat/src/intent.rs`'s `DECIDE_CALL_TIMEOUT` cites: measured in this deployment,
/// `Decide`'s p50 is about 155 ms against `Complete`'s p50 of about 2000 ms
/// (`llm_router.decisions.latency_ms` and `llm_router.requests.latency_ms`). 2 s covers a cold
/// connection and this service's own hop out to llm-router and back — not a generous budget for
/// the vendor, a bound on how long this node waits before falling back to today's flow.
/// `services/tool/src/service.rs`'s `VALIDATE_TIMEOUT` reasons the same way but is deliberately
/// longer, since that path has no fallback to fall back to.
///
/// **Must stay strictly above `services/llm-router`'s `adapters::system_one::REQUEST_TIMEOUT`
/// (1.5 s), for the same reason Chat's matching constant must**: this deadline is asserted as the
/// `grpc-timeout` header llm-router's connectrpc server parses on receipt, wrapping the *entire*
/// dispatch to the vendor in a deadline that starts strictly before the adapter's own clock does.
/// Equal or shorter and this outer deadline always wins that race, silently dropping llm-router's
/// decision future — audit write included — before the inner timeout ever finishes.
const DECIDE_CALL_TIMEOUT: Duration = Duration::from_secs(2);
/// A wrong or slow tool dispatched on the fast path is exactly one tool call, so it gets the same
/// budget the normal `tool` node gives one — see `executors::tool::CALL_TIMEOUT`, which this
/// mirrors (that constant is private to its own module, so this is its own copy, not a shared one).
const TOOL_CALL_TIMEOUT: Duration = Duration::from_secs(60);

const PROMISE_ID: &str = "promise";
/// Above this the reply undertakes work rather than reporting any. Set at the midpoint: the guard
/// only ever replaces a promise with the truth, so an even split is better spent on the honest
/// answer than on a sentence that reads like success and is not.
const PROMISE_THRESHOLD: f64 = 0.5;
const PROMISE_INSTRUCTIONS: &str = "Does this text undertake to do something, rather than report \
something already done or known?";
const PROMISE_WHEN_TRUE: &str = "it says what it is about to do, will do, or needs to do — work \
that has not happened yet";
const PROMISE_WHEN_FALSE: &str = "it states what is the case, gives a value, answers, asks the \
person something, or says plainly that it could not find out";
/// What replaces a promise. It reports the failure, because the undertaking will not be kept: the
/// reply ends the turn.
const PROMISE_REFUSED: &str = "I couldn't get that just now. Ask me again and I'll try.";
const TOOL_QUESTION_ID: &str = "tool";
const NO_TOOL_OPTION: &str = "none";

/// The one question the whole turn now turns on. It asks which of the sources the operator has
/// connected covers what is being asked — a question about the catalog — and not whether the model
/// needs help, which is a question about the model's confidence in its own memory.
///
/// That distinction is the whole change. A calibrated decider asked "does this require looking
/// something up?" answers no whenever the model believes it knows, and believing it knows is
/// exactly the state in which it is most likely to be confidently wrong. Measured: a question whose
/// answer the connected store held was answered from memory, incorrectly, because that question
/// came back confidently false.
///
/// Phrased naming no subject: the repo's `gate-architecture` rule "Capabilities, Not Topics"
/// forbids a domain or topic anywhere in code or prompts. Every subject in this decision arrives
/// from the catalog at runtime, written by the operator.
const TOOL_QUESTION_INSTRUCTIONS: &str = "Which of these sources can answer this question? Choose \
the one whose material covers what is being asked. Choose the option for none of them when none \
does.";
/// The `none` option's own description. Without it the decider is left to infer what `none` means
/// from its name, and the option that has to carry every out-of-scope question is the one that can
/// least afford to be guessed at.
/// The decline's own words. They name what can be answered and ask which of it was meant, because a
/// person who asked something adjacent needs a way back in — a bare refusal leaves them guessing at
/// what the system is for.
const DECLINE_OPENING: &str = "I can only answer from what I'm connected to: ";
const DECLINE_CLOSING: &str = "Which of those did you mean?";
/// When the catalog is empty there is nothing to offer, and listing nothing would read as a bug.
const NOTHING_CONNECTED: &str =
    "I'm not connected to anything I can answer from at the moment. Please try again shortly.";
const NO_TOOL_DESCRIPTION: &str =
    "no source listed here holds what this question asks for, or the message asks for nothing";

/// A pick at or above this is acted on. Measured over 16 questions against a catalog whose entries
/// describe their coverage: every in-scope question reached its source at 0.88 or better, and every
/// out-of-scope one reached `none` at 0.91 or better. The gap is wide enough that the exact value
/// here does not decide anything — which is the point of asking the catalog rather than the model.
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

/// Question ids for an argument whose value is a span of the message. Each carries the tool's own
/// name, because the catalog may offer several such arguments and all of them are asked
/// speculatively in the one request — the candidate spans depend on the message, not on which tool
/// turns out to win, so there is nothing to wait for.
const VALUE_QUESTION_PREFIX: &str = "value::";
const VALUE_STATED_PREFIX: &str = "stated::";
const VALUE_ONE_ONLY_PREFIX: &str = "one_only::";
/// The escape every closed set gets: the options are spans of one message, and the message may
/// simply not contain the value.
const NO_VALUE_OPTION: &str = "none";
/// A pick that loses to its neighbours — "York" against "New York" — lands here rather than being
/// dispatched, and the turn composes the argument generatively as it does today.
const VALUE_CHOICE_CONFIDENCE_THRESHOLD: f64 = 0.6;
/// Absolute, and asked about the message alone: a relative choice ranks *something* first even when
/// the message names nothing. Measured, this is what catches "the weather in the capital of
/// Kyrgyzstan" (0.15) and "the weather here" (0.05), where the choice itself is confident and wrong.
const VALUE_STATED_THRESHOLD: f64 = 0.5;
/// Also absolute, and the one that catches "weather in Paris and Berlin" (0.08), where the value is
/// stated outright — twice. Two literal questions combined in code, rather than one question
/// carrying both judgements.
const VALUE_ONE_ONLY_THRESHOLD: f64 = 0.5;
/// How many adjacent words may form one candidate, so a value of more than one word — "New York",
/// "Rio de Janeiro" — is among the options rather than only its parts.
const MAX_SPAN_WORDS: usize = 3;
/// Kept under the decision vendor's 255-option ceiling with room to spare. A message long enough to
/// exceed it is not one where a single argument is being named anyway, so it falls through.
const MAX_SPAN_OPTIONS: usize = 200;
/// How many message-valued arguments are asked about speculatively in one request. Extra questions
/// are nearly free, but a catalog is not a licence to send an unbounded number of them.
const MAX_SPAN_ARGUMENTS: usize = 4;

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
    /// The slug a call is dispatched by.
    pub name: String,
    /// What the operator calls this source, for the one place a person reads it: the decline that
    /// names what can be answered. A slug there reads as a machine dump, and a person told
    /// `kb_read_document` has been told nothing.
    #[serde(default)]
    pub title: String,
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
        });
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
    // The style block is about a reply, and it is the last thing the model reads. In compose-only
    // there is no reply to style, and closing with "a bare value is a complete answer" undoes the
    // instruction the mode opened with.
    if mode != Mode::ComposeOnly {
        system.push_str(FINAL_ANSWER_STYLE);
    }

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

/// The first `{tool_call, reply}` object in `content`, when the whole of `content` would not parse
/// as one. A light model sometimes writes a sentence before the envelope, or two envelopes in a
/// row; whole-string parsing fails on both and the fallback then shows the person the machine text
/// verbatim — observed live as `Bishkek. {"tool_call": {...}, "reply": null}` in a chat window.
///
/// Only a parse succeeding from a `{` counts, so ordinary prose containing a brace is untouched and
/// still becomes the reply.
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

        Ok(output)
    }
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

/// What one typed `Decide` call, sent before the first generative call of a turn, resolves to.
/// `decide_usage` is `Decide`'s own token cost, shaped for `merge_usage` — see
/// `decide_usage_value` — so `try_fast_path` can fold it into the budget the same way
/// `merge_usage` already folds the nudge's second `Complete` in.
enum FastPath {
    /// `tool` came back confident, named a live catalog entry, that entry's schema takes exactly
    /// one required string field, and the question is shaped like something safe to send
    /// verbatim: call it with the question verbatim.
    ToolCall { call: ToolCall, decide_usage: Value },
    /// `tool` came back confident and named a live catalog entry, but its arguments could not be
    /// settled by the decision alone. The model is asked for the call and nothing else — it is
    /// not offered the choice of answering, because that is the choice it gets wrong: given both,
    /// a light model replies "I will look that up" and the turn ends having called nothing.
    ComposeOnly { tool: String, decide_usage: Value },
    /// No connected source covers the question — `none`, confidently — or the decision could not
    /// be reached at all. Both end the turn the same way, saying what can be answered and asking
    /// which of it was meant. They are one variant because they are one outcome for the person:
    /// nothing here was answered, and nothing was invented in place of an answer.
    ///
    /// A decision that does not arrive used to fall through to a loop where the model could answer
    /// from memory. That fall-through is gone: it is the failure this design exists to remove, and
    /// keeping it for outages would mean the guarantee holds only while the provider is up.
    Decline { decide_usage: Value },
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
            FastPath::ComposeOnly { tool, decide_usage } => Some(
                self.compose_tool_call(config, state, &tool, &decide_usage, idempotency_key)
                    .await,
            ),
            FastPath::Decline { decide_usage } => Some(Ok(decline(config, &decide_usage))),
        }
    }

    /// The last thing between a promise and the person. Twice offered tools and twice answering
    /// without calling one, the model has either answered from what it knows — which is fine — or
    /// undertaken to do something, which is not: nothing runs after a reply, so the undertaking is
    /// never kept and the turn ends looking like it succeeded.
    ///
    /// Only a typed decision can tell the two apart, and it is asked about the finished reply
    /// rather than the question. A promise is replaced with the plain truth. An unavailable or
    /// unconfident decision leaves the reply exactly as it was: this guard may only remove a
    /// promise, never invent a failure.
    async fn without_an_empty_promise(&self, output: Value) -> Value {
        let Some(reply) = output.get("reply").and_then(Value::as_str).filter(|reply| {
            !reply.is_empty() && output.get("tool_call").is_none_or(Value::is_null)
        }) else {
            return output;
        };
        let Some(request) = promise_request(reply) else {
            return output;
        };
        // The client already carries DECIDE_CALL_TIMEOUT, so a slow decider costs the same bounded
        // wait here as anywhere else, and a failure leaves the reply untouched.
        let decided = match self.decider.decide(request).await {
            Ok(response) => response.into_owned(),
            Err(error) => {
                tracing::warn!(%error, "could not check the reply for a promise, leaving it as it is");
                return output;
            }
        };
        if !find_noul(&decided.answers, PROMISE_ID).is_some_and(|value| value > PROMISE_THRESHOLD) {
            return output;
        }
        tracing::info!("the reply undertook work that never ran, answering plainly instead");
        let usage = decide_usage_value(&decided);
        let mut replaced = output;
        if let Some(object) = replaced.as_object_mut() {
            object.insert("reply".to_owned(), json!(PROMISE_REFUSED));
        }
        merge_usage(&usage, replaced)
    }

    /// One `Complete` in compose-only mode, then the dispatch the decision already committed to.
    /// A response carrying no `tool_call` is refused and asked again once with the refusal stated;
    /// a second refusal is answered honestly rather than with the promise the model wanted to
    /// make, because a promise ends the turn and nothing keeps it.
    async fn compose_tool_call(
        &self,
        config: &LlmNodeConfig,
        state: &Value,
        tool: &str,
        decide_usage: &Value,
        idempotency_key: &str,
    ) -> Result<Value, TaskError> {
        let request = complete_request_for(config, state, Mode::ComposeOnly);
        let mut spent = self.complete_with_retries(&request).await?;
        if composed_call(&spent).is_none() {
            tracing::info!(
                tool,
                "compose-only reply carried no tool call, asking once more"
            );
            let mut again = request.clone();
            again.system_prompt.push_str(COMPOSE_REFUSAL);
            let second = self.complete_with_retries(&again).await?;
            spent = merge_usage(&spent, second);
        }
        let Some(call) = composed_call(&spent).filter(|call| {
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
        }) else {
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
        let result = self.dispatch_tool_fast(&call, idempotency_key).await;
        let state_with_result = state_with_extra_tool_result(state, result.clone());
        // Loop, not answer-only: a failed call is shown as an ERROR result, and only the loop's
        // instructions say that this means call again rather than that the task is over.
        let request = complete_request(config, &state_with_result);
        let output = self.complete_with_retries(&request).await?;
        let output = self.without_an_empty_promise(output).await;
        let output = merge_usage(decide_usage, merge_usage(&spent, output));
        Ok(attach_fast_tool_call(output, &call, result))
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
        let output = self.without_an_empty_promise(output).await;
        let output = merge_usage(decide_usage, output);
        Ok(attach_fast_tool_call(output, call, result))
    }

    /// Sends one `Decide` whose state is the question alone — not the whole execution state, per
    /// the vendor's own guidance. Any error or timeout is `FastPath::Decline`: the turn says what it
    /// that keeps a `Decide` outage from ever costing a turn. The vendor evaluates each question
    /// independently (`llm_router.proto`'s `DecideRequest` doc), so a confident `tool` pick and a
    /// confident "no information needed" can — and do — co-occur; `confident_tool_choice` is
    /// checked first for exactly that reason, and a confident pick that fails the fast-dispatch
    /// gate still returns `Unavailable`, never falling through to `NoToolNeeded`.
    async fn decide_fast_path(&self, config: &LlmNodeConfig, question: &str) -> FastPath {
        let Some(request) = decide_request(question, &config.available_tools) else {
            return FastPath::Decline {
                decide_usage: Value::Null,
            };
        };
        let decided = match self.decider.decide(request).await {
            Ok(response) => response.into_owned(),
            Err(error) => {
                tracing::warn!(
                    %error,
                    "typed decision failed, falling back to today's flow with the nudge enabled"
                );
                return FastPath::Decline {
                    decide_usage: Value::Null,
                };
            }
        };
        let decide_usage = decide_usage_value(&decided);

        if let Some(choice) = confident_tool_choice(&decided.answers) {
            return match fast_dispatchable_tool_call(
                choice,
                &config.available_tools,
                question,
                &decided.answers,
            ) {
                Some(call) => FastPath::ToolCall { call, decide_usage },
                // The tool is needed and it exists; only its arguments are open. That is a
                // composing job, not a decision to revisit.
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
    request.questions = vec![tool_question(available_tools)?];
    request
        .questions
        .extend(decidable_questions(question, available_tools));
    Some(request)
}

/// The message's own words, and every run of up to `MAX_SPAN_WORDS` adjacent words, deduplicated.
/// Trailing punctuation is trimmed so `Bishkek?` offers `Bishkek`, which is the normalisation step
/// the pattern expects of the code around the pick, not a rule about what a value looks like.
fn spans(message: &str) -> Vec<String> {
    let words: Vec<&str> = message
        .split_whitespace()
        .map(|word| word.trim_matches(|c: char| !c.is_alphanumeric()))
        .filter(|word| !word.is_empty())
        .collect();
    let mut found: Vec<String> = Vec::new();
    for size in 1..=MAX_SPAN_WORDS {
        for run in words.windows(size) {
            let candidate = run.join(" ");
            if !found.contains(&candidate) {
                found.push(candidate);
            }
        }
    }
    found
}

/// `Some((field, description))` when a tool's schema names exactly one required string field and
/// that field says its value appears among the words of the message. The description is the
/// field's own, so the question asks about the meaning the operator wrote rather than about the
/// parameter's name — a question named after its parameter gives the message nothing to match.
/// The field and its description, when a typed decision can settle its value at all — whichever
/// kind of options it turns out to have.
fn decidable_argument(input_schema_json: &str) -> Option<(String, String)> {
    let (field, description, _) = decidable_field(input_schema_json)?;
    Some((field, description))
}

/// Where a decidable argument's options come from. A closed list is the stronger claim of the two:
/// the schema itself says what the endpoint will accept, so whatever comes back is a value it
/// accepts. A span is the weaker one: the value is a word of the message, which the message might
/// not contain at all.
enum Options {
    /// A closed list, optionally with a line per value — `oneOf` entries of `{const, description}`,
    /// or a bare `enum` of strings.
    Closed(Vec<(String, Option<String>)>),
    /// Built per message, from its own words.
    Spans,
}

/// `Some((field, description, options))` when a tool's schema names exactly one required string
/// field whose value a typed decision can settle: either because the schema lists what it may be,
/// or because the field says it appears among the words of the message. The description is the
/// field's own, so the question asks about the meaning the operator wrote rather than about the
/// parameter's name — a question named after its parameter gives the message nothing to match.
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

/// A property's closed list, if it has one. `oneOf` of `{const, description}` is preferred because
/// it carries a line per value, which is what lets the decision tell two near-identical symbols
/// apart; a bare `enum` of strings works and simply offers no such line.
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

/// Three questions per eligible argument, all over the same message and all in the one request the
/// turn already sends: which span is the value, whether the message states it at all, and whether
/// it states only one. The first is relative and will rank something regardless; the other two are
/// absolute and are what make a wrong pick visible.
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
        .take(MAX_SPAN_ARGUMENTS)
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

/// The span the decision picked for `tool`, once all three of its questions agree it is safe to
/// use: the choice is confident and is not the escape, the message states the value outright, and
/// it states only one. Any of those failing leaves the argument to be composed generatively, which
/// costs one call and nothing else.
fn picked_value(answers: &[Answer], tool: &str) -> Option<String> {
    let choice = find_choice(answers, &format!("{VALUE_QUESTION_PREFIX}{tool}"))?;
    if choice.choice == NO_VALUE_OPTION
        || choice.confidence < VALUE_CHOICE_CONFIDENCE_THRESHOLD
        || find_noul(answers, &format!("{VALUE_STATED_PREFIX}{tool}"))? < VALUE_STATED_THRESHOLD
        || find_noul(answers, &format!("{VALUE_ONE_ONLY_PREFIX}{tool}"))? < VALUE_ONE_ONLY_THRESHOLD
    {
        return None;
    }
    Some(choice.choice.clone())
}

fn instructed(id: &str, instructions: Value) -> Option<Question> {
    let mut question: Question =
        serde_json::from_value(json!({ "instructions": instructions })).ok()?;
    question.id = id.to_owned();
    Some(question)
}

/// Asked of the finished reply, never of the question. Phrased about what the sentence does, so it
/// names no subject and no tool — the `gate-architecture` rule holds here as everywhere.
fn promise_request(reply: &str) -> Option<DecideRequest> {
    let mut request: DecideRequest = serde_json::from_value(json!({ "state": reply })).ok()?;
    let mut question = instructed(PROMISE_ID, json!(PROMISE_INSTRUCTIONS))?;
    question.kind = Noul {
        when_true: Some(PROMISE_WHEN_TRUE.to_owned()),
        when_false: Some(PROMISE_WHEN_FALSE.to_owned()),
        ..Default::default()
    }
    .into();
    request.questions = vec![question];
    Some(request)
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
    answers: &[Answer],
) -> Option<ToolCall> {
    let tool = available_tools
        .iter()
        .find(|tool| tool.name == choice.choice)?;

    // An argument that takes a word or two of the message is tried first: it is the narrower claim
    // of the two, and the same decision already carries its answer.
    if let Some((field, _)) = decidable_argument(&tool.input_schema_json)
        && let Some(value) = picked_value(answers, &tool.name)
    {
        return Some(ToolCall {
            name: tool.name.clone(),
            args: json!({ field: value }),
        });
    }

    if !fits_fast_dispatch(question) {
        return None;
    }
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
/// one field, that field's own schema is `"type": "string"`, and it is annotated as taking free
/// text. A tool needing structured arguments — two required fields, a non-string field, or a schema
/// this cannot parse — is `None`, so it never takes the fast path regardless of how confidently it
/// was chosen.
///
/// The annotation is the load-bearing part, and it is read from the tool's own declaration rather
/// than decided here, so a new capability is still a file the operator writes and not a branch in
/// this function. Without it, an argument that wants a value — a place, a pair, a code — is handed
/// the user's whole sentence, and the request was never going to work: measured at ~850 ms spent
/// reaching a third party that rejects it, against ~321 ms for the generative call that composes
/// the argument properly. See `common::tool_schema::ACCEPTS_FREE_TEXT` for why absent means no.
fn single_required_string_field(input_schema_json: &str) -> Option<String> {
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

/// Appended when a compose-only response came back without a call. It states the refusal rather
/// than repeating the instruction: the model has already read the instruction once.
const COMPOSE_REFUSAL: &str = r#"

Your previous response was refused: it carried no tool call. Saying that you will look something
up does nothing — nothing runs after your reply. Call the tool now.
"#;

/// What the turn says when the model would not compose the call twice. It reports the failure
/// instead of promising the work, because a promise is what the model was trying to send and no
/// promise made here is ever kept.
const COMPOSE_FAILURE: &str = "I couldn't work out what to look up for that. Could you say it \
                               again with the name in it?";

/// Whether `choice` names a tool the live catalog actually carries. A pick for something absent is
/// not a composing job — there is nothing to compose arguments for.
fn names_a_live_tool(choice: &ChoiceAnswer, available_tools: &[CatalogEntry]) -> bool {
    available_tools
        .iter()
        .any(|tool| tool.name == choice.choice)
}

/// What the turn says when no connected source covers the question. Composed from the catalog's own
/// `name`s at runtime — the code names nothing, and a deployment that connects different sources
/// gets a different sentence without being rebuilt.
///
/// No generative call is spent on it. Asking a model to phrase the refusal costs a second and
/// invites it to answer the question inside the refusal, which is the failure being refused.
fn decline(config: &LlmNodeConfig, decide_usage: &Value) -> Value {
    let mut names: Vec<&str> = Vec::new();
    for tool in &config.available_tools {
        // The slug is the fallback, not the choice: a catalog that publishes no title leaves the
        // person something to name rather than nothing.
        let name = Some(tool.title.as_str())
            .filter(|title| !title.is_empty())
            .unwrap_or(tool.name.as_str());
        // Several tools can reach one source — searching it and reading from it are two entries in
        // the catalog and one thing to a person. An operator names them alike, and naming the same
        // thing twice in a list reads as a fault.
        if !names.contains(&name) {
            names.push(name);
        }
    }
    let reply = if names.is_empty() {
        NOTHING_CONNECTED.to_owned()
    } else {
        format!("{DECLINE_OPENING}{}. {DECLINE_CLOSING}", listed(&names))
    };
    merge_usage(
        decide_usage,
        json!({"tool_call": Value::Null, "reply": reply}),
    )
}

/// Joins names the way a sentence does, so the reply reads as one rather than as a dump.
fn listed(names: &[&str]) -> String {
    match names {
        [] => String::new(),
        [only] => (*only).to_owned(),
        [rest @ .., last] => format!("{}, and {last}", rest.join(", ")),
    }
}

/// The `tool_call` a compose-only response carries, if it carries one.
fn composed_call(output: &Value) -> Option<ToolCall> {
    let written: LlmOutput = serde_json::from_value(output.clone()).ok()?;
    serde_json::from_value(written.tool_call).ok()
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

    // A search that attached the readable text of its top two hits (Task C: a search returns the
    // pages it found) produces one tool result whose combined text runs well past the old 4 000
    // char cap — raised so those pages actually reach the model instead of being cut before the
    // model ever sees them. Two pages of 5 000 chars each (10 000 total) must survive whole.
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

    /// Observed live: the model wrote a sentence and then the envelope, and the person was shown
    /// `Bishkek. {"tool_call": {...}, "reply": null}` as the answer. The call is what it meant.
    #[test]
    fn parse_llm_output_recovers_an_envelope_written_after_a_sentence() {
        let output = parse_llm_output(
            r#"Bishkek. {"tool_call": {"name": "weather_now", "args": {"place": "Bishkek"}}, "reply": null}"#,
        );
        assert_eq!(output["tool_call"]["name"], json!("weather_now"));
    }

    /// Two envelopes in a row parse as neither. The first is the one that was asked for.
    #[test]
    fn parse_llm_output_takes_the_first_of_two_envelopes() {
        let output = parse_llm_output(
            r#"{"tool_call": {"name": "weather_now", "args": {}}, "reply": null}
{"tool_call": {"name": "currency_rate", "args": {}}, "reply": null}"#,
        );
        assert_eq!(output["tool_call"]["name"], json!("weather_now"));
    }

    /// Prose that merely contains a brace is still prose, and is still the reply.
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
        /// The span pick, its confidence, and the two absolute gates, answered for whichever tool
        /// is asked about: `(choice, confidence, stated, one_only)`.
        decide_span: Mutex<Option<(String, f64, f64, f64)>>,
        decide_needs_external_info: Mutex<Option<f64>>,
        decide_promise: Mutex<Option<f64>>,
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
                decide_span: Mutex::new(None),
                decide_needs_external_info: Mutex::new(None),
                decide_promise: Mutex::new(None),
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
                decide_span: Mutex::new(None),
                decide_needs_external_info: Mutex::new(None),
                decide_promise: Mutex::new(None),
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

        /// The system prompt of the nth `Complete`, so a test can check what a mode offered.
        fn system_prompt(&self, nth: usize) -> String {
            self.received.lock().expect("lock")[nth]
                .system_prompt
                .clone()
        }

        /// Arms `Decide`'s `tool` answer. Leaving both this and
        /// `answer_decide_needs_external_info` unset makes `decide` fail, matching an unarmed
        /// `Complete` — the fail-safe every non-scripted existing test in this module exercises.
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

        /// Arms the guard's verdict over a finished reply.
        fn answer_decide_promise(&self, noul: f64) {
            *self.decide_promise.lock().expect("lock") = Some(noul);
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
            let span = self.decide_span.lock().expect("lock").clone();
            let promise = *self.decide_promise.lock().expect("lock");
            if tool.is_none() && promise.is_none() {
                return Err(connectrpc::ConnectError::unavailable(
                    "configured fake decider failure",
                ));
            }
            let owned = request.to_owned_message();
            let answers = owned
                .questions
                .iter()
                .filter_map(|question| {
                    if question.id == PROMISE_ID {
                        return promise.map(|noul| noul_answer(PROMISE_ID, noul));
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

        /// The arguments the first dispatch carried, so a test can tell a composed call from one
        /// that was handed the question verbatim.
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

    /// A tool whose one required argument is the asking words themselves — the only shape that may
    /// be handed a message verbatim, and the only shape the fast path accepts.
    fn single_field_tool(name: &str, field: &str) -> CatalogEntry {
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

    /// The same shape but for an argument that wants a value rather than a query. It qualifies on
    /// every count the fast path used to check, and must still not be dispatched verbatim.
    fn single_value_field_tool(name: &str, field: &str) -> CatalogEntry {
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

    /// The generative loop as it now exists: reached only once a source has run, where the state
    /// already carries a result. Before that, the turn either dispatches or declines, so there is
    /// no longer any way to arrive here on a bare question.
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
            "properties": {"query": {"type": "string", "x-accepts-free-text": true}},
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
                "query": {"type": "string", "x-accepts-free-text": true},
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

    /// A value of more than one word must be among the options, not only its parts, or the pick can
    /// only ever be half of it.
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

    /// The escape every closed set gets: the options are spans of one message, and the message may
    /// not contain the value at all.
    #[test]
    fn the_pick_offers_a_way_to_say_none_of_these() {
        let questions = decidable_questions(
            "weather in Bishkek?",
            &[span_field_tool("some_tool", "place")],
        );
        let rendered = serde_json::to_string(&questions[0]).expect("serialize");
        assert!(rendered.contains(NO_VALUE_OPTION), "{rendered}");
    }

    /// A schema that lists what the endpoint accepts is the stronger claim of the two: whatever the
    /// decision returns is a value the endpoint takes, because it could only choose from that list.
    /// No annotation is needed — JSON Schema already has the vocabulary.
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

    /// A bare `enum` works too and simply offers no line per value.
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

    /// The listed values win over the message's words when a schema says both: the schema knows
    /// what the endpoint accepts, and the message only knows what was typed.
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

    /// A field that says neither is left alone, and its argument is composed as before.
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

    /// Each gate catches a different way of being confidently wrong, measured against the live
    /// decider: a value that is only implied ("the capital of Kyrgyzstan"), one that is not there at
    /// all ("here"), and two of them at once ("Paris and Berlin"). The choice is confident in all
    /// three; only the absolute questions tell them apart.
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

    // The shape the whole annotation exists for: one required string field that wants a *value* —
    // a place, a pair, a code — not the asking words. Handing it the message verbatim produces a
    // request that was never going to work, so it must not qualify however confidently the tool
    // was chosen. Absent is the default, which is why this schema says nothing at all.
    #[test]
    fn single_required_string_field_rejects_a_string_that_does_not_take_free_text() {
        let schema = json!({
            "type": "object",
            "required": ["place"],
            "properties": {"place": {"type": "string"}},
        })
        .to_string();
        assert_eq!(single_required_string_field(&schema), None);
    }

    #[test]
    fn single_required_string_field_rejects_free_text_declared_as_anything_but_true() {
        let schema = json!({
            "type": "object",
            "required": ["place"],
            "properties": {"place": {"type": "string", "x-accepts-free-text": "yes"}},
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

    /// End to end: the argument is a word of the message, the decision picks it, and the tool is
    /// reached with that word — no generative call composes anything, and the whole question is
    /// nowhere near the tool.
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

    /// The same tool, the same confident choice, but the gates refuse the pick. Nothing is
    /// dispatched and the turn composes the argument the way it does today.
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

    /// The counterpart, end to end: a tool whose one required string argument wants a value rather
    /// than the asking words is *not* dispatched verbatim, however confidently it was chosen. It
    /// satisfies every other condition the fast path checks, so only the annotation stands between
    /// the turn and a request that was never going to work.
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
            available_tools: vec![single_value_field_tool("some_tool", "place")],
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

    /// The question the whole design turns on: a source covers it, or nothing does. When nothing
    /// does, the turn says so and names what it can answer — and spends no generative call doing
    /// it, because asking a model to phrase a refusal invites it to answer inside the refusal.
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
        let mut entry = single_field_tool("some_slug", "query");
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

    /// Several catalog entries can reach one source: searching it and reading from it are two rows
    /// and one thing to the person reading the decline.
    #[tokio::test]
    async fn the_decline_names_one_source_once_however_many_tools_reach_it() {
        let fake = Arc::new(FakeLlmRouter::always(
            r#"{"tool_call": null, "reply": "x"}"#,
        ));
        fake.answer_decide_tool(NO_TOOL_OPTION, 0.97);
        let llm_url = serve(Arc::clone(&fake)).await;
        let executor = LlmTaskExecutor::new(&llm_url, UNREACHABLE).expect("client");
        let mut searching = single_field_tool("search_it", "query");
        searching.title = "The stored material".to_owned();
        let mut reading = single_field_tool("read_it", "query");
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

    /// The guarantee cannot hold only while the decider is up. A decision that never arrives used
    /// to fall through to a loop where the model answered from memory; that was the failure, and an
    /// outage is exactly when it would go unnoticed.
    #[tokio::test]
    async fn a_decision_that_never_arrives_declines_rather_than_answering_from_memory() {
        let fake = Arc::new(FakeLlmRouter::always(
            r#"{"tool_call": null, "reply": "Bishkek."}"#,
        ));
        // Nothing arms the decider, so `decide` fails the way an outage fails.
        let llm_url = serve(Arc::clone(&fake)).await;
        let tool = Arc::new(FakeToolService::ok("{}"));
        let tool_url = serve_tool(Arc::clone(&tool)).await;
        let executor = LlmTaskExecutor::new(&llm_url, &tool_url).expect("client");
        let config = LlmNodeConfig {
            tool_calling: true,
            available_tools: vec![single_field_tool("Current weather for a place", "query")],
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

    /// A pick the live catalog does not carry is not a source. It cannot be dispatched and it
    /// cannot license an answer, so it declines like any other unanswerable question.
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
            available_tools: vec![single_field_tool("Current weather for a place", "query")],
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

    /// A lean is not a pick. Below the threshold no source has been named, and naming none is what
    /// declining means.
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
            available_tools: vec![single_field_tool("Current weather for a place", "query")],
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

    /// With nothing connected there is nothing to offer, and listing nothing would read as a bug.
    #[tokio::test]
    async fn an_empty_catalog_says_so_rather_than_listing_nothing() {
        let fake = Arc::new(FakeLlmRouter::always(
            r#"{"tool_call": null, "reply": "x"}"#,
        ));
        let llm_url = serve(Arc::clone(&fake)).await;
        let executor = LlmTaskExecutor::new(&llm_url, UNREACHABLE).expect("client");
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

    /// The promise guard now sits where a promise is still possible: after a source has run, over
    /// the reply that was written from its result.
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
            available_tools: vec![single_field_tool("Current weather for a place", "query")],
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

    /// A tool whose schema needs two fields cannot be handed the question verbatim, but the
    /// decision that a tool is needed still stands. The model composes the arguments; what it must
    /// not be given back is the choice of whether to call at all.
    #[tokio::test]
    async fn a_tool_needing_two_fields_is_composed_rather_than_dispatched_verbatim() {
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

    /// The whole point of the compose-only mode: a light model handed both options answers "I will
    /// look that up", which runs nothing and ends the turn. Refused and asked again, it calls.
    #[tokio::test]
    async fn a_compose_only_reply_without_a_call_is_refused_and_asked_again() {
        let fake = Arc::new(FakeLlmRouter::then(
            r#"{"tool_call": null, "reply": "Sure, I will look that up for you."}"#,
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
            1,
            "the promise is refused and the second response's call is dispatched"
        );
        assert!(
            fake.system_prompt(1).contains("was refused"),
            "and the second ask states the refusal rather than repeating the instruction"
        );
    }

    /// Twice refused, the turn says so. It must not send the promise instead: nothing runs after a
    /// reply, so a promise made here is never kept.
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

    // The vendor evaluates `tool` and `needs_external_information` independently, so a confident
    // tool pick that the schema gate disqualifies and a confidently-false need-for-information can
    // co-occur. A confident tool pick must veto NoToolNeeded too, not just block ToolCall.
    #[tokio::test]
    async fn a_confident_schema_disqualified_tool_pick_vetoes_no_tool_needed_too() {
        let fake = Arc::new(FakeLlmRouter::always(
            r#"{"tool_call": {"name": "two_field_tool", "args": {"a": "x", "b": "y"}}}"#,
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

    // A follow-up's question is not always a question — see MAX_FAST_DISPATCH_QUESTION_CHARS's
    // doc. A confident, schema-eligible tool pick must still not be fast-dispatched when the
    // question itself is multi-line, and must still veto NoToolNeeded (today's flow decides).
    #[tokio::test]
    async fn a_multi_line_question_is_not_fast_dispatched_even_when_the_tool_otherwise_qualifies() {
        let fake = Arc::new(FakeLlmRouter::always(
            r#"{"tool_call": {"name": "some_tool", "args": {"query": "population of Paris"}}}"#,
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
