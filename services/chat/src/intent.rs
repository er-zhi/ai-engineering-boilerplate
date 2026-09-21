// Routes one user turn with one typed decision. Three questions ride the same call because a second
// question is far cheaper than a second round trip, and the whole point here is that the user sees
// something back immediately.

use std::sync::{LazyLock, OnceLock};
use std::time::Duration;

use buffa::EnumValue;
use common::proto::llm_router::v1::{
    Answer, Choice, ChoiceAnswer, ChoiceOption, CompleteRequest, DecideRequest, DecisionBudget,
    DescribeModelsRequest, LlmRouterServiceClient, Noul, QualityTier, Question, ResponseFormat,
    Sampling, SystemOneServiceClient, answer::Answer as Given,
};
use connectrpc::Protocol;
use connectrpc::client::{ClientConfig, HttpClient};
use serde::{Deserialize, Serialize};

use crate::entity::topic::Status;
use crate::topic_status::status_word;

/// `Decide` gets its own deadline, separate from `Complete`'s (see `COMPLETE_CALL_TIMEOUT` below —
/// the two must never again share one constant). Measured in this deployment, `Decide`'s p50 is
/// about 155 ms against `Complete`'s p50 of about 2000 ms (`llm_router.decisions.latency_ms` and
/// `llm_router.requests.latency_ms`). 2 s leaves wide headroom above that p50, because real latency
/// can still stack up before an answer arrives: a cold connection (no warm pool yet), and our own
/// hop out to llm-router and back. `Decide` is advisory — see `route`'s doc and `fallback` below —
/// so this bounds how long a user waits for the deterministic fallback to kick in on a bad day, not
/// how generously the vendor is normally treated.
///
/// **This is the OUTER half of a matched pair with llm-router's own
/// `adapters::system_one::REQUEST_TIMEOUT` (the INNER one, 1.5 s) — the inner must stay strictly
/// below this one, and if you change one, change both.** This client asserts this deadline on the
/// wire as the `grpc-timeout` header; `connectrpc`'s server on llm-router's side parses it on
/// receipt and wraps that whole side's dispatch — including the pending vendor HTTP call the inner
/// timeout bounds — in its own `timeout_at`, started the moment the request lands there, strictly
/// earlier than the inner adapter's own clock (which only starts once it actually issues its HTTP
/// call). Two equal deadlines with an earlier and a later start are not a race: the outer one —
/// this one — always wins, and when it does, llm-router drops that whole future, including its
/// audit write, before that write ever runs — silently losing the very latency measurement this
/// budget was tuned from. Keep this strictly above the inner one, with margin.
const DECIDE_CALL_TIMEOUT: Duration = Duration::from_secs(2);
/// `Complete` produces generated text, not a typed decision, and keeps the older, more generous
/// budget this timeout always had. Unrelated to, and unaffected by, `DECIDE_CALL_TIMEOUT` above —
/// each client below is built with its own `ClientConfig`, so changing one can never silently move
/// the other.
const COMPLETE_CALL_TIMEOUT: Duration = Duration::from_secs(30);
pub const MAX_TITLE_CHARS: usize = 60;

/// Getting this wrong is not symmetric: asking a person to repeat themselves costs one extra
/// exchange, while letting an agent execution run a full minute on a bare greeting costs the whole
/// exchange. So the turn only stops short once the model is fairly confident (under 35%) that it
/// carries no request at all, rather than clarifying at the first sign of doubt.
const ACTIONABLE_THRESHOLD: f64 = 0.35;
/// Below this the model cannot separate the options at all, and the deterministic focus rule is a
/// better answer than its guess.
const ROUTE_CONFIDENCE_THRESHOLD: f64 = 0.5;
/// Splitting a message that holds one theme costs a completion and produces two half-topics, so
/// this asks for real certainty.
const SEPARATE_THEMES_THRESHOLD: f64 = 0.75;
/// Below this the split dropped something, and the turn is answered as one topic carrying the whole
/// message instead. A split is a convenience — themes answered separately, each arriving as it is
/// ready — while a dropped theme is an answer the person never gets, so the two are not traded off
/// against each other: the split is kept only when it is known to have kept everything.
///
/// Why a check rather than a better prompt: counting the themes, cutting the message and rewriting
/// each part are three judgements in one generative call, and a lighter model makes them worse. The
/// rewriting genuinely needs a generative model; noticing that a part went missing does not.
const COVERS_EVERYTHING_THRESHOLD: f64 = 0.5;

pub const CLARIFICATION_TEXT: &str =
    "I didn't catch a request in that — what would you like me to find out?";

/// Shown when a follow-up arrives while its topic's execution is still running and Engine's
/// `Interrupt` reports back busy rather than accepting it — see `topic_turn.rs::send_turn`. This is
/// the interim answer to a message that could not be delivered: it tells the user honestly rather
/// than losing the turn behind an `internal` error, without queueing it for them.
pub const ENGINE_BUSY_TEXT: &str =
    "Still working on the previous question — send that again in a moment.";

const NEW_TOPIC_OPTION: &str = "new";
const TOPIC_OPTION_PREFIX: &str = "topic_";

const ROUTE_ID: &str = "route";
const ACTIONABLE_ID: &str = "actionable";
const SEPARATE_THEMES_ID: &str = "separate_themes";
const COVERS_EVERYTHING_ID: &str = "covers_everything";

const ROUTE_INSTRUCTIONS: &str = "Which of these should the new message go to: one of the named \
existing topics it continues, or a new topic?";
const ACTIONABLE_INSTRUCTIONS: &str =
    "Does the new message, on its own, carry a request the assistant should act on?";
const ACTIONABLE_WHEN_TRUE: &str =
    "it states or clearly implies something to find out, do, or answer";
const ACTIONABLE_WHEN_FALSE: &str = "it is a bare fragment, acknowledgment, or nudge — e.g. \
\"and?\", \"i'm still waiting\", \"more details please\", \"that's not what I asked\" — with no \
request of its own";
const SEPARATE_THEMES_INSTRUCTIONS: &str =
    "Does the new message raise more than one independent theme, each deserving its own topic?";
const SEPARATE_THEMES_WHEN_TRUE: &str =
    "the message names two or more unrelated subjects, each a separate task on its own";
const SEPARATE_THEMES_WHEN_FALSE: &str = "the message is about one subject, however long";
const COVERS_EVERYTHING_INSTRUCTIONS: &str = "Between them, do the questions in `questions` ask \
for everything `message` asks for?";
const COVERS_EVERYTHING_WHEN_TRUE: &str =
    "every distinct thing the message asks about is asked for by one of the questions";
const COVERS_EVERYTHING_WHEN_FALSE: &str = "the message asks about something none of the \
questions asks for, so answering all of them would leave part of the message unanswered";

const EXAMPLE_QUESTION_1: &str = "What is Claude Code?";
const EXAMPLE_QUESTION_2: &str = "Which Academy courses exist?";
const ELLIPSIS: &str = "…";

static SPLIT_SYSTEM_PROMPT: LazyLock<String> = LazyLock::new(|| {
    format!(
        "{SPLIT_PROMPT_RULES}- `title` is a short label (at most {MAX_TITLE_CHARS} characters) \
         for the topic list.\n{}",
        *SPLIT_PROMPT_EXAMPLES
    )
});

const SPLIT_PROMPT_RULES: &str = "\
A message can name several separate, independent themes at once. Your job is to split it into one \
new topic per theme.

Rules:
- Open one new topic per genuinely separate theme. Do not split a single theme into multiple \
topics just because it has several parts.
- For every topic, `question` must be the user's OWN request for that theme, first person, \
near-verbatim — copy their wording for that part of the message.
- A topic is answered on its own, with nothing but its `question`, so a theme that says \"there\", \
\"it\" or \"that one\" must say instead what the rest of the message says it is. Take those words \
from the message and no further: \"the capital of Kyrgyzstan, and the weather there\" gives \"what \
is the weather in the capital of Kyrgyzstan?\" — never \"in Bishkek\", which the message does not \
say. Answering the other theme is not your job and guessing its answer wrongly would be invisible \
from here.
- Never write a paraphrased instruction about \"the user\" (e.g. never \"the user asked about X, \
please answer it\") — `question` is what the user themselves would have typed, not a description \
of their request.
";

static SPLIT_PROMPT_EXAMPLES: LazyLock<String> = LazyLock::new(|| {
    let new_topic = |title: &str, question: &str| RawAction::New {
        title: Some(title.to_owned()),
        question: Some(question.to_owned()),
    };
    format!(
        "
Example:

User (first message of the session): \"{EXAMPLE_QUESTION_1} And {EXAMPLE_QUESTION_2}\"
{}

Answer with strict JSON and nothing else, in exactly this shape:
{}
At least one action. Several actions are allowed.",
        plan_shape(vec![
            new_topic("Claude Code", EXAMPLE_QUESTION_1),
            new_topic("Academy courses", EXAMPLE_QUESTION_2),
        ]),
        plan_shape(vec![new_topic(ELLIPSIS, ELLIPSIS)]),
    )
});

#[derive(Clone, Debug)]
pub struct TopicSummary {
    pub id: i64,
    pub title: String,
    pub status: Status,
    pub result_summary: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Continue { topic_id: i64 },
    New { title: String, question: String },
}

/// What one turn is routed to. `Clarify` is reached only when nothing existing was chosen and the
/// message itself carried no actionable request — see `TopicIntent::route`.
pub enum Routing {
    /// Nothing to act on. No topic is created and no execution starts.
    Clarify,
    Act(Vec<Action>),
}

#[derive(Deserialize, Serialize)]
struct RawPlan {
    actions: Vec<serde_json::Value>,
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum RawAction {
    Continue {
        topic_id: i64,
    },
    New {
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        question: Option<String>,
    },
}

#[derive(Serialize)]
struct PlanShape<'a> {
    actions: &'a [RawAction],
}

fn plan_shape(actions: Vec<RawAction>) -> String {
    serde_json::to_string(&PlanShape { actions: &actions }).expect("a RawAction always serializes")
}

pub struct TopicIntent {
    llm: LlmRouterServiceClient<HttpClient>,
    decider: SystemOneServiceClient<HttpClient>,
    /// `route`'s option ceiling, read once from `DescribeModels` — see `load_decision_budget` below
    /// — and never refreshed after. `OnceLock` because it is written at most once, from Chat's
    /// startup path in `main.rs` before any turn is routed, and then only ever read — concurrently,
    /// by every turn after that, with no lock contention on the hot path. Unset (never loaded, or
    /// `DescribeModels` failed) is handled identically to a loaded-but-zero budget: see
    /// `route_options_cap`, which treats both as "no known cap" rather than "send no options".
    decision_budget: OnceLock<DecisionBudget>,
}

impl TopicIntent {
    pub fn new(llm_router_url: &str) -> Result<Self, String> {
        let target = llm_router_url
            .parse()
            .map_err(|e| format!("could not parse LLM_ROUTER_URL {llm_router_url:?}: {e}"))?;
        let base_config = ClientConfig::new(target)
            .with_protocol(Protocol::Grpc)
            .proto();
        Ok(Self {
            llm: LlmRouterServiceClient::new(
                HttpClient::plaintext_http2_only(),
                base_config
                    .clone()
                    .with_default_timeout(COMPLETE_CALL_TIMEOUT),
            ),
            decider: SystemOneServiceClient::new(
                HttpClient::plaintext_http2_only(),
                base_config.with_default_timeout(DECIDE_CALL_TIMEOUT),
            ),
            decision_budget: OnceLock::new(),
        })
    }

    /// Reads the decision budget once, called from Chat's startup path (`main.rs`) strictly before
    /// any turn is routed — never lazily, so no turn ever pays for this call on the latency path.
    /// Every failure — an unreachable router, an error response, a malformed reply — is logged at
    /// `warn` and leaves the budget unset. This must never fail or panic: `Decide` is advisory (see
    /// `route`'s doc), so the budget describing it is too, and a router that is down at boot must
    /// not stop Chat from serving turns it would route by fallback anyway. A second call is a
    /// silent no-op — `OnceLock::set` keeps whatever the first call wrote — matching "read once".
    pub async fn load_decision_budget(&self) {
        match self
            .decider
            .describe_models(DescribeModelsRequest::default())
            .await
        {
            Ok(response) => {
                let budget = response
                    .into_owned()
                    .budget
                    .into_option()
                    .unwrap_or_default();
                let _ = self.decision_budget.set(budget);
            }
            Err(error) => {
                tracing::warn!(
                    %error,
                    "could not read the decision budget at startup; routing with no option cap"
                );
            }
        }
    }

    /// `route`'s live option ceiling, derived from whatever `load_decision_budget` last read.
    /// `None` — nothing was ever loaded, or what was loaded named no positive ceiling — means "no
    /// known cap": the proto default for an absent or unset budget is zero, and a zero ceiling must
    /// not be read as "offer nothing" (see `capped_topics`'s doc for why that is safe).
    fn route_options_cap(&self) -> Option<usize> {
        self.decision_budget
            .get()
            .and_then(|budget| usize::try_from(budget.max_choice_options).ok())
            .filter(|cap| *cap > 0)
    }

    /// Routes one user turn. See the module doc for the read order of the three answers — it is
    /// load-bearing, not an implementation detail to simplify away.
    pub async fn route(
        &self,
        topics: &[TopicSummary],
        focus: Option<i64>,
        message: &str,
    ) -> Routing {
        let Some(request) = build_request(topics, focus, message, self.route_options_cap()) else {
            return Routing::Act(fallback(topics, focus, message));
        };

        let decided = match self.decider.decide(request).await {
            Ok(response) => response.into_owned(),
            Err(error) => {
                tracing::warn!(%error, "topic intent decision failed");
                return Routing::Act(fallback(topics, focus, message));
            }
        };
        if decided.answers.is_empty() {
            return Routing::Act(fallback(topics, focus, message));
        }

        if !topics.is_empty() {
            match route_outcome(&decided.answers, topics) {
                RouteOutcome::Continue(topic_id) => {
                    return Routing::Act(vec![Action::Continue { topic_id }]);
                }
                RouteOutcome::Fallback => return Routing::Act(fallback(topics, focus, message)),
                RouteOutcome::ConfidentNew => {}
            }
        }

        if noul_above(
            &decided.answers,
            SEPARATE_THEMES_ID,
            SEPARATE_THEMES_THRESHOLD,
        ) && let Some(actions) = self.split(topics, message).await
        {
            return Routing::Act(actions);
        }

        if noul_below(&decided.answers, ACTIONABLE_ID, ACTIONABLE_THRESHOLD) {
            return Routing::Clarify;
        }

        Routing::Act(vec![Action::New {
            title: truncate(message, MAX_TITLE_CHARS),
            question: message.to_owned(),
        }])
    }

    // The one branch that still needs generated text: several themes in one message, each becoming
    // its own topic. Every other outcome is read straight off the typed decision.
    async fn split(&self, topics: &[TopicSummary], message: &str) -> Option<Vec<Action>> {
        let response = self
            .llm
            .complete(CompleteRequest {
                tier: EnumValue::Known(QualityTier::Low),
                system_prompt: SPLIT_SYSTEM_PROMPT.clone(),
                user_prompt: format!("New user message:\n{message}\n"),
                sampling: Sampling {
                    response_format: Some(EnumValue::Known(ResponseFormat::JsonObject)),
                    ..Default::default()
                }
                .into(),
                ..Default::default()
            })
            .await
            .ok()?;
        let actions = parse_actions(&response.into_owned().content, topics)?;
        self.keeps_everything(message, &actions)
            .await
            .then_some(actions)
    }

    /// Whether a split left anything behind. Asked of the decider, over the message and the
    /// questions the split produced, and answered `false` when the decision is unavailable — a
    /// split nobody vouched for is not worth the answer it might lose.
    async fn keeps_everything(&self, message: &str, actions: &[Action]) -> bool {
        let questions: Vec<&str> = actions
            .iter()
            .filter_map(|action| match action {
                Action::New { question, .. } => Some(question.as_str()),
                Action::Continue { .. } => None,
            })
            .collect();
        if questions.len() < 2 {
            return true;
        }
        let Some(question) = covers_everything_question() else {
            return false;
        };
        let state = serde_json::json!({ "message": message, "questions": questions });
        let Ok(request) =
            serde_json::from_value::<DecideRequest>(serde_json::json!({ "state": state }))
        else {
            return false;
        };
        let request = DecideRequest {
            questions: vec![question],
            ..request
        };
        match self.decider.decide(request).await {
            Ok(response) => noul_above(
                &response.into_owned().answers,
                COVERS_EVERYTHING_ID,
                COVERS_EVERYTHING_THRESHOLD,
            ),
            Err(error) => {
                tracing::warn!(%error, "could not check that a split kept every theme");
                false
            }
        }
    }
}

fn covers_everything_question() -> Option<Question> {
    let mut question = instructed(
        COVERS_EVERYTHING_ID,
        serde_json::json!(COVERS_EVERYTHING_INSTRUCTIONS),
    )?;
    question.kind = Noul {
        when_true: Some(COVERS_EVERYTHING_WHEN_TRUE.to_owned()),
        when_false: Some(COVERS_EVERYTHING_WHEN_FALSE.to_owned()),
        ..Default::default()
    }
    .into();
    Some(question)
}

/// The JSON `Decide` reads the topics and message about. An array, not an object, because a
/// `google.protobuf.Value` object's keys reach the model re-sorted alphabetically (it is a map),
/// and topic order — the focused one first — is part of what is being asked about.
fn decision_state(topics: &[TopicSummary], focus: Option<i64>, message: &str) -> serde_json::Value {
    let mut ordered: Vec<&TopicSummary> = Vec::with_capacity(topics.len());
    ordered.extend(topics.iter().filter(|topic| Some(topic.id) == focus));
    ordered.extend(topics.iter().filter(|topic| Some(topic.id) != focus));
    let topics_json: Vec<serde_json::Value> = ordered
        .into_iter()
        .map(|topic| {
            serde_json::json!({
                "id": topic.id,
                "title": topic.title,
                "status": status_word(topic.status),
                "focused": Some(topic.id) == focus,
            })
        })
        .collect();
    serde_json::Value::Array(vec![
        serde_json::json!({ "topics": topics_json }),
        serde_json::json!({ "message": message }),
    ])
}

fn build_request(
    topics: &[TopicSummary],
    focus: Option<i64>,
    message: &str,
    options_cap: Option<usize>,
) -> Option<DecideRequest> {
    let mut questions = Vec::new();
    if !topics.is_empty() {
        questions.push(route_question(topics, options_cap)?);
    }
    questions.push(actionable_question()?);
    questions.push(separate_themes_question()?);

    let state = decision_state(topics, focus, message);
    let mut request: DecideRequest =
        serde_json::from_value(serde_json::json!({ "state": state })).ok()?;
    request.questions = questions;
    Some(request)
}

fn instructed(id: &str, instructions: serde_json::Value) -> Option<Question> {
    let mut question: Question =
        serde_json::from_value(serde_json::json!({ "instructions": instructions })).ok()?;
    question.id = id.to_owned();
    Some(question)
}

/// Builds the `route` question: one option per offered topic, plus `new`. `options_cap` — the
/// budget's `max_choice_options`, read once at startup (see `TopicIntent::load_decision_budget`) —
/// bounds the whole option list, `new` included; see `capped_topics` for which topics are dropped
/// and why dropping one is safe.
fn route_question(topics: &[TopicSummary], options_cap: Option<usize>) -> Option<Question> {
    let mut options: Vec<ChoiceOption> = capped_topics(topics, options_cap)
        .iter()
        .map(|topic| ChoiceOption {
            name: format!("{TOPIC_OPTION_PREFIX}{}", topic.id),
            description: Some(topic.title.clone()),
            ..Default::default()
        })
        .collect();
    options.push(ChoiceOption {
        name: NEW_TOPIC_OPTION.to_owned(),
        description: None,
        ..Default::default()
    });
    let mut question = instructed(ROUTE_ID, serde_json::json!(ROUTE_INSTRUCTIONS))?;
    question.kind = Choice {
        options,
        ..Default::default()
    }
    .into();
    Some(question)
}

/// Keeps at most `options_cap` topics, preferring the most recent, and always leaves one slot free
/// for the `new` option `route_question` appends. `topics` arrives in ascending creation order
/// (`SessionManager::get_session_view` orders by `CreatedAt` ascending), so "most recent" is the
/// tail of the slice. `None` — no cap was ever loaded, or the router published none — returns every
/// topic, exactly today's uncapped behaviour.
///
/// Dropping a topic from the options never makes it unreachable: `fallback` below continues the
/// session's focused topic regardless of what `route` was ever asked about, so a topic left out
/// here is only unofferable this turn, not unreachable — that is what makes the cap safe to apply.
fn capped_topics(topics: &[TopicSummary], options_cap: Option<usize>) -> &[TopicSummary] {
    let Some(cap) = options_cap else {
        return topics;
    };
    let keep = cap.saturating_sub(1).min(topics.len());
    &topics[topics.len() - keep..]
}

fn actionable_question() -> Option<Question> {
    let mut question = instructed(ACTIONABLE_ID, serde_json::json!(ACTIONABLE_INSTRUCTIONS))?;
    question.kind = Noul {
        when_true: Some(ACTIONABLE_WHEN_TRUE.to_owned()),
        when_false: Some(ACTIONABLE_WHEN_FALSE.to_owned()),
        ..Default::default()
    }
    .into();
    Some(question)
}

fn separate_themes_question() -> Option<Question> {
    let mut question = instructed(
        SEPARATE_THEMES_ID,
        serde_json::json!(SEPARATE_THEMES_INSTRUCTIONS),
    )?;
    question.kind = Noul {
        when_true: Some(SEPARATE_THEMES_WHEN_TRUE.to_owned()),
        when_false: Some(SEPARATE_THEMES_WHEN_FALSE.to_owned()),
        ..Default::default()
    }
    .into();
    Some(question)
}

enum RouteOutcome {
    /// A confident pick of an existing topic — returned before `actionable` is ever consulted.
    Continue(i64),
    /// The model could not separate the options, or named something that does not exist.
    Fallback,
    /// A confident `new` — proceed to `separate_themes` / `actionable`.
    ConfidentNew,
}

enum RouteChoice {
    Topic(i64),
    New,
}

fn parse_route_choice(choice: &str) -> Option<RouteChoice> {
    if choice == NEW_TOPIC_OPTION {
        return Some(RouteChoice::New);
    }
    choice
        .strip_prefix(TOPIC_OPTION_PREFIX)
        .and_then(|id| id.parse().ok())
        .map(RouteChoice::Topic)
}

/// Reads `route` first. This is the ordering the module doc calls load-bearing: putting `route`
/// ahead of `actionable` means a fragment that plainly continues a live topic — "and?", "i'm still
/// waiting" — never reaches the clarification check, because it is never actionable on its own.
fn route_outcome(answers: &[Answer], topics: &[TopicSummary]) -> RouteOutcome {
    let Some(choice) = find_choice(answers, ROUTE_ID) else {
        return RouteOutcome::Fallback;
    };
    if choice.confidence < ROUTE_CONFIDENCE_THRESHOLD {
        return RouteOutcome::Fallback;
    }
    match parse_route_choice(&choice.choice) {
        Some(RouteChoice::Topic(topic_id)) if topics.iter().any(|topic| topic.id == topic_id) => {
            RouteOutcome::Continue(topic_id)
        }
        Some(RouteChoice::New) => RouteOutcome::ConfidentNew,
        Some(RouteChoice::Topic(_)) | None => RouteOutcome::Fallback,
    }
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

fn noul_above(answers: &[Answer], id: &str, threshold: f64) -> bool {
    find_noul(answers, id).is_some_and(|value| value > threshold)
}

fn noul_below(answers: &[Answer], id: &str, threshold: f64) -> bool {
    find_noul(answers, id).is_some_and(|value| value < threshold)
}

#[must_use]
fn parse_actions(content: &str, topics: &[TopicSummary]) -> Option<Vec<Action>> {
    let plan: RawPlan = serde_json::from_str(outermost_json_object(content)?).ok()?;
    let actions: Vec<Action> = plan
        .actions
        .into_iter()
        .filter_map(|raw| serde_json::from_value::<RawAction>(raw).ok())
        .filter_map(|raw| match raw {
            RawAction::Continue { topic_id } => topics
                .iter()
                .any(|topic| topic.id == topic_id)
                .then_some(Action::Continue { topic_id }),
            RawAction::New { title, question } => {
                let question = question.filter(|q| !q.trim().is_empty())?;
                let title = title
                    .filter(|t| !t.trim().is_empty())
                    .unwrap_or_else(|| question.clone());
                Some(Action::New {
                    title: truncate(&title, MAX_TITLE_CHARS),
                    question,
                })
            }
        })
        .collect();
    (!actions.is_empty()).then_some(actions)
}

fn outermost_json_object(content: &str) -> Option<&str> {
    let start = content.find('{')?;
    let end = content.rfind('}')?;
    (end > start).then(|| &content[start..=end])
}

/// The behaviour the service had before any decision existed: continue the focus if it is still a
/// live topic, otherwise open one new topic from the message verbatim. Every failure mode of
/// `route` — unreachable router, an error response, no answers, an answer for an id never asked, a
/// `topic_<id>` outside the session, a low-confidence route — lands here. Classification is
/// advisory; a turn is never lost to it.
#[must_use]
pub(crate) fn fallback(topics: &[TopicSummary], focus: Option<i64>, message: &str) -> Vec<Action> {
    match focus {
        Some(topic_id) if topics.iter().any(|topic| topic.id == topic_id) => {
            vec![Action::Continue { topic_id }]
        }
        _ => vec![Action::New {
            title: truncate(message, MAX_TITLE_CHARS),
            question: message.to_owned(),
        }],
    }
}

#[must_use]
pub fn truncate(text: &str, max_chars: usize) -> String {
    text.trim().chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    const CLARIFY_BELOW: f64 = ACTIONABLE_THRESHOLD - 0.1;

    /// A local mirror of llm-router's `adapters::system_one::REQUEST_TIMEOUT` (the INNER half of
    /// the matched pair `DECIDE_CALL_TIMEOUT` documents above). This crate has no dependency on
    /// `llm-router` to check against the real constant, so this pins the value the two are kept
    /// in sync with by hand; llm-router's own `system_one` tests pin the same value from its
    /// side. Keep both in sync with the real `system_one::REQUEST_TIMEOUT` when either changes.
    const LLM_ROUTER_DECIDE_REQUEST_TIMEOUT_MIRROR: Duration = Duration::from_millis(1500);

    // The invariant a lost audit row was traced to: connectrpc's server on llm-router's side
    // starts its own deadline (this outer constant) strictly before llm-router's adapter starts
    // its own inner one, so equal or larger and the outer always wins the race, dropping
    // llm-router's decision future — audit write included — before it runs. Pins that the outer
    // stays strictly above the inner.
    #[test]
    fn the_outer_chat_deadline_stays_strictly_above_llm_routers_inner_one() {
        assert!(
            DECIDE_CALL_TIMEOUT > LLM_ROUTER_DECIDE_REQUEST_TIMEOUT_MIRROR,
            "Chat's outer Decide deadline must stay strictly above llm-router's own inner \
             vendor-call timeout, or the outer one always wins the race and llm-router's audit \
             row for that attempt is silently never written: {DECIDE_CALL_TIMEOUT:?} vs \
             {LLM_ROUTER_DECIDE_REQUEST_TIMEOUT_MIRROR:?}"
        );
    }

    fn topics() -> Vec<TopicSummary> {
        vec![TopicSummary {
            id: 7,
            title: "An earlier topic, still running".to_owned(),
            status: crate::entity::topic::Status::Running,
            result_summary: None,
        }]
    }

    #[tokio::test]
    async fn nothing_to_act_on_and_nothing_to_continue_asks() {
        // No topics at all, so `route` is not even sent: a greeting in an empty session is the
        // clarification case.
        let (url, calls) = crate::fakes::serve_decider(vec![("actionable", CLARIFY_BELOW)]).await;
        let intent = TopicIntent::new(&url).expect("client");
        assert!(matches!(
            intent.route(&[], None, "hey").await,
            Routing::Clarify
        ));
        assert_eq!(
            calls.completions(),
            0,
            "a clarification must not spend a completion"
        );
    }

    #[tokio::test]
    async fn a_fragment_aimed_at_a_live_topic_continues_it_instead_of_being_questioned() {
        // "and?" carries no request of its own, so `actionable` is low — but it plainly continues
        // the running topic, and interrogating the user about it is the failure this ordering
        // exists to prevent.
        let (url, _) = crate::fakes::serve_decider_choosing_with(
            "topic_7",
            0.9,
            vec![("actionable", CLARIFY_BELOW)],
        )
        .await;
        let intent = TopicIntent::new(&url).expect("client");
        let Routing::Act(actions) = intent.route(&topics(), Some(7), "and?").await else {
            panic!("a fragment aimed at a live topic must never clarify");
        };
        assert_eq!(actions, vec![Action::Continue { topic_id: 7 }]);
    }

    #[tokio::test]
    async fn a_confident_route_continues_that_topic() {
        let (url, _) = crate::fakes::serve_decider_choosing("topic_7", 0.9).await;
        let intent = TopicIntent::new(&url).expect("client");
        let Routing::Act(actions) = intent.route(&topics(), Some(7), "and?").await else {
            panic!("expected actions");
        };
        assert_eq!(actions, vec![Action::Continue { topic_id: 7 }]);
    }

    #[tokio::test]
    async fn a_new_topic_keeps_the_users_own_words_verbatim() {
        let (url, calls) = crate::fakes::serve_decider_choosing("new", 0.9).await;
        let intent = TopicIntent::new(&url).expect("client");
        let Routing::Act(actions) = intent
            .route(&topics(), Some(7), "What is Rust's latest stable version?")
            .await
        else {
            panic!("expected actions");
        };
        assert_eq!(
            actions,
            vec![Action::New {
                title: "What is Rust's latest stable version?".to_owned(),
                question: "What is Rust's latest stable version?".to_owned(),
            }]
        );
        assert_eq!(calls.completions(), 0, "one theme needs no completion");
    }

    #[tokio::test]
    async fn a_title_longer_than_the_limit_is_truncated_but_the_question_is_not() {
        let long = "x".repeat(MAX_TITLE_CHARS + 40);
        let (url, _) = crate::fakes::serve_decider_choosing("new", 0.9).await;
        let intent = TopicIntent::new(&url).expect("client");
        let Routing::Act(actions) = intent.route(&[], None, &long).await else {
            panic!("expected actions");
        };
        let Action::New { title, question } = &actions[0] else {
            panic!("expected a new topic");
        };
        assert_eq!(title.chars().count(), MAX_TITLE_CHARS);
        assert_eq!(question, &long);
    }

    /// A split that loses a theme is not a faster answer, it is a missing one. When the decider
    /// will not vouch that the questions cover the message, the turn becomes one topic carrying the
    /// whole message — slower, and complete.
    #[tokio::test]
    async fn a_split_that_drops_a_theme_is_refused_and_the_whole_message_is_answered() {
        let (url, _calls) = crate::fakes::serve_decider_themes_covering(
            0.9,
            0.05,
            r#"{"actions":[{"kind":"new","title":"A","question":"first theme?"},
                           {"kind":"new","title":"B","question":"second theme?"}]}"#,
        )
        .await;
        let intent = TopicIntent::new(&url).expect("client");
        let message = "first theme? and second theme? and a third one nobody wrote down";

        let Routing::Act(actions) = intent.route(&[], None, message).await else {
            panic!("expected actions");
        };

        assert_eq!(actions.len(), 1, "{actions:?}");
        let Action::New { question, .. } = &actions[0] else {
            panic!("expected a new topic");
        };
        assert_eq!(question, message, "the whole message, so nothing is lost");
    }

    #[tokio::test]
    async fn several_themes_are_the_one_case_that_completes() {
        let (url, calls) = crate::fakes::serve_decider_splitting(
            r#"{"actions":[{"kind":"new","title":"Claude Code","question":"What is Claude Code?"},
                           {"kind":"new","title":"Academy","question":"Which Academy courses exist?"}]}"#,
        )
        .await;
        let intent = TopicIntent::new(&url).expect("client");
        let Routing::Act(actions) = intent
            .route(
                &[],
                None,
                "What is Claude Code? And which Academy courses exist?",
            )
            .await
        else {
            panic!("expected actions");
        };
        assert_eq!(actions.len(), 2);
        assert_eq!(calls.completions(), 1);
    }

    // Real network on loopback (the fake server, the TCP and HTTP/2 handshake), virtual time for
    // the deadline itself: `start_paused` only auto-advances the clock once nothing else is ready
    // to run, so the handshake and request still complete at wall-clock speed, and only the
    // "server never responds" wait is fast-forwarded.
    #[tokio::test(start_paused = true)]
    async fn a_decider_that_never_answers_is_abandoned_within_the_new_deadline() {
        let url = crate::fakes::serve_decider_hanging().await;
        let intent = TopicIntent::new(&url).expect("client");

        let started = tokio::time::Instant::now();
        let Routing::Act(actions) = intent.route(&topics(), Some(7), "and?").await else {
            panic!("a hung decider must still fall back, not clarify");
        };
        assert_eq!(actions, vec![Action::Continue { topic_id: 7 }]);

        let elapsed = started.elapsed();
        assert!(
            elapsed >= DECIDE_CALL_TIMEOUT,
            "returned before the deadline even elapsed: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(10),
            "abandoned near the 2s deadline, not the old 30s one: {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn an_unreachable_decider_continues_the_focus_instead_of_clarifying() {
        let intent = TopicIntent::new("http://127.0.0.1:1").expect("client");
        let Routing::Act(actions) = intent.route(&topics(), Some(7), "and?").await else {
            panic!("a failure must never clarify");
        };
        assert_eq!(actions, vec![Action::Continue { topic_id: 7 }]);
    }

    #[tokio::test]
    async fn a_topic_id_that_is_not_in_the_session_falls_back_to_the_focus() {
        let (url, _) = crate::fakes::serve_decider_choosing("topic_999", 0.9).await;
        let intent = TopicIntent::new(&url).expect("client");
        let Routing::Act(actions) = intent.route(&topics(), Some(7), "and?").await else {
            panic!("expected actions");
        };
        assert_eq!(actions, vec![Action::Continue { topic_id: 7 }]);
    }

    #[tokio::test]
    async fn an_unsure_route_continues_the_focus() {
        let (url, _) = crate::fakes::serve_decider_choosing("new", 0.2).await;
        let intent = TopicIntent::new(&url).expect("client");
        let Routing::Act(actions) = intent.route(&topics(), Some(7), "and?").await else {
            panic!("expected actions");
        };
        assert_eq!(actions, vec![Action::Continue { topic_id: 7 }]);
    }

    #[test]
    fn the_state_is_an_array_so_the_topic_order_survives() {
        let state = decision_state(&topics(), Some(7), "and?");
        assert!(
            state.is_array(),
            "an object's keys reach the model re-sorted: {state}"
        );
    }

    // `many_topics` mirrors the order `SessionManager::get_session_view` hands `route`: ascending
    // by creation, oldest first — so the highest id here is the most recently created topic.
    fn many_topics(count: i64) -> Vec<TopicSummary> {
        (1..=count)
            .map(|id| TopicSummary {
                id,
                title: format!("Topic {id}"),
                status: crate::entity::topic::Status::Running,
                result_summary: None,
            })
            .collect()
    }

    fn topic_option(id: i64) -> String {
        format!("{TOPIC_OPTION_PREFIX}{id}")
    }

    fn choice_option_names(question: &Question) -> Vec<String> {
        match question.kind.as_ref() {
            Some(common::proto::llm_router::v1::question::Kind::Choice(choice)) => {
                choice.options.iter().map(|o| o.name.clone()).collect()
            }
            _ => Vec::new(),
        }
    }

    // The "computed" half of the cap: route_question itself, exercised directly rather than
    // through a network round trip, so this pins the cap arithmetic (exactly the cap, `new`
    // included, most recent kept) independently of whether anything ever wires the cap in.
    #[test]
    fn route_options_are_capped_at_the_budget_keeping_the_most_recent_plus_new() {
        let topics = many_topics(5); // ids 1..=5, 5 is the most recently created
        let question =
            route_question(&topics, Some(3)).expect("a route question with topics present");
        let names = choice_option_names(&question);

        assert_eq!(
            names.len(),
            3,
            "a cap of 3 must yield exactly 3 options: {names:?}"
        );
        assert!(
            names.contains(&NEW_TOPIC_OPTION.to_owned()),
            "new must always be offered: {names:?}"
        );
        assert!(
            names.contains(&topic_option(5)) && names.contains(&topic_option(4)),
            "the two most recent topics must be kept: {names:?}"
        );
        assert!(
            !names.contains(&topic_option(1)),
            "the oldest topic must be the one dropped: {names:?}"
        );
    }

    #[test]
    fn no_cap_offers_every_topic_exactly_as_before() {
        let topics = many_topics(5);
        let question = route_question(&topics, None).expect("a route question");
        assert_eq!(choice_option_names(&question).len(), 6, "5 topics plus new");
    }

    // The "actually sent" half of the cap: goes through TopicIntent::route and a real (fake)
    // network round trip, so this catches a bug where the cap is computed and stored but the
    // request-building path that reaches the wire never receives it.
    #[tokio::test]
    async fn the_route_options_cap_reaches_what_actually_leaves_the_process() {
        let router = Arc::new(crate::fakes::FakeLlmRouter::default());
        router.answer_decision(Some(("new", 0.9)), vec![("actionable", 0.9)]);
        router.answer_describe_models(DecisionBudget {
            max_choice_options: 3,
            ..Default::default()
        });
        let url = crate::fakes::serve_router(Arc::clone(&router)).await;
        let intent = TopicIntent::new(&url).expect("client");
        intent.load_decision_budget().await;

        let topics = many_topics(5);
        let _ = intent.route(&topics, None, "hello").await;

        let sent = router.last_route_options();
        assert_eq!(
            sent.len(),
            3,
            "the wire request must carry exactly the loaded cap, not the uncapped list: {sent:?}"
        );
        assert!(sent.contains(&NEW_TOPIC_OPTION.to_owned()), "{sent:?}");
        assert!(
            sent.contains(&topic_option(5)) && sent.contains(&topic_option(4)),
            "the most recent topics must be the ones offered: {sent:?}"
        );
    }

    // Task 3's other required test: DescribeModels being unavailable at startup must not stop
    // Chat from working. load_decision_budget must not panic or hang, and a turn afterward must
    // still route on the uncapped default — proving the router's Decide path is unaffected by its
    // DescribeModels path failing.
    #[tokio::test]
    async fn describe_models_being_unavailable_at_startup_leaves_chat_working_on_the_defaults() {
        let router = Arc::new(crate::fakes::FakeLlmRouter::default());
        router.answer_decision(Some(("new", 0.9)), vec![("actionable", 0.9)]);
        router.fail_describe_models();
        let url = crate::fakes::serve_router(Arc::clone(&router)).await;
        let intent = TopicIntent::new(&url).expect("client");

        intent.load_decision_budget().await;

        let topics = many_topics(5);
        let Routing::Act(actions) = intent.route(&topics, None, "hello").await else {
            panic!("a turn must still route even though the startup budget fetch failed");
        };
        assert_eq!(
            actions,
            vec![Action::New {
                title: "hello".to_owned(),
                question: "hello".to_owned(),
            }]
        );

        let sent = router.last_route_options();
        assert_eq!(
            sent.len(),
            topics.len() + 1,
            "no cap was ever loaded, so nothing is trimmed: {sent:?}"
        );
    }

    // An unreachable router at startup — not just a router that answers with an error — must be
    // just as harmless: load_decision_budget must return normally rather than hang or panic, and
    // routing afterward must fall back exactly as it always has.
    #[tokio::test]
    async fn an_unreachable_router_at_startup_still_lets_a_turn_route_by_fallback() {
        let intent = TopicIntent::new("http://127.0.0.1:1").expect("client");

        intent.load_decision_budget().await;

        let Routing::Act(actions) = intent.route(&topics(), Some(7), "and?").await else {
            panic!("a failure must never clarify");
        };
        assert_eq!(actions, vec![Action::Continue { topic_id: 7 }]);
    }
}
