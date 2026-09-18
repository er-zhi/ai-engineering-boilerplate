// Routes one user turn with one typed decision. Three questions ride the same call because a second
// question is far cheaper than a second round trip, and the whole point here is that the user sees
// something back immediately.

use std::sync::LazyLock;
use std::time::Duration;

use buffa::EnumValue;
use common::proto::llm_router::v1::{
    Answer, Choice, ChoiceAnswer, ChoiceOption, CompleteRequest, DecideRequest,
    LlmRouterServiceClient, Noul, QualityTier, Question, ResponseFormat, Sampling,
    SystemOneServiceClient, answer::Answer as Given,
};
use connectrpc::Protocol;
use connectrpc::client::{ClientConfig, HttpClient};
use serde::{Deserialize, Serialize};

use crate::entity::topic::Status;
use crate::topic_status::status_word;

/// `Decide` gets its own deadline, separate from `Complete`'s (see `COMPLETE_CALL_TIMEOUT` below —
/// the two must never again share one constant). The vendor's own docs put a typical answer at
/// about 100 ms, with 0.27 s published for a 13-question call. This is 20x that headline number,
/// not the usual 2x, because three things can each add real latency before any of the vendor's own
/// answer arrives: a cold connection (no warm pool yet), a retry the vendor's SDK performs
/// internally before it ever reports back to `services/llm-router`, and our own hop out to
/// llm-router and back. `Decide` is advisory — see `route`'s doc and `fallback` below — so this
/// bounds how long a user waits for the deterministic fallback to kick in on a bad day, not how
/// generously the vendor is normally treated.
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

pub const CLARIFICATION_TEXT: &str =
    "I didn't catch a request in that — what would you like me to find out?";

const NEW_TOPIC_OPTION: &str = "new";
const TOPIC_OPTION_PREFIX: &str = "topic_";

const ROUTE_ID: &str = "route";
const ACTIONABLE_ID: &str = "actionable";
const SEPARATE_THEMES_ID: &str = "separate_themes";

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
near-verbatim — copy their wording for that part of the message. Only minimally expand it with \
necessary context that is missing from the message itself (e.g. a place or name mentioned \
elsewhere in the message). Never write a paraphrased instruction about \"the user\" (e.g. never \
\"the user asked about X, please answer it\") — `question` is what the user themselves would have \
typed, not a description of their request.
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
        })
    }

    /// Routes one user turn. See the module doc for the read order of the three answers — it is
    /// load-bearing, not an implementation detail to simplify away.
    pub async fn route(
        &self,
        topics: &[TopicSummary],
        focus: Option<i64>,
        message: &str,
    ) -> Routing {
        let Some(request) = build_request(topics, focus, message) else {
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
        parse_actions(&response.into_owned().content, topics)
    }
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
) -> Option<DecideRequest> {
    let mut questions = Vec::new();
    if !topics.is_empty() {
        questions.push(route_question(topics)?);
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

fn route_question(topics: &[TopicSummary]) -> Option<Question> {
    let mut options: Vec<ChoiceOption> = topics
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

    const CLARIFY_BELOW: f64 = ACTIONABLE_THRESHOLD - 0.1;

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
}
