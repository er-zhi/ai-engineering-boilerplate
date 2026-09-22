// Routes one user turn onto new or existing topics with one typed decision.

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

const DECIDE_CALL_TIMEOUT: Duration = Duration::from_secs(2);
const COMPLETE_CALL_TIMEOUT: Duration = Duration::from_secs(30);
pub const MAX_TITLE_CHARS: usize = 60;

const CLARIFY_WHEN_ACTIONABLE_BELOW: f64 = 0.35;
const CONTINUE_FOCUS_WHEN_ROUTE_CONFIDENCE_BELOW: f64 = 0.5;
const SPLIT_WHEN_SEPARATE_THEMES_ABOVE: f64 = 0.75;
const KEEP_SPLIT_WHEN_COVERS_EVERYTHING_ABOVE: f64 = 0.5;
const REWRITE_WHEN_STANDS_ALONE_BELOW: f64 = 0.5;

pub const CLARIFICATION_TEXT: &str =
    "I didn't catch a request in that — what would you like me to find out?";

pub const ENGINE_BUSY_TEXT: &str =
    "Still working on the previous question — send that again in a moment.";

const NEW_TOPIC_OPTION: &str = "new";
const TOPIC_OPTION_PREFIX: &str = "topic_";

const ROUTE_ID: &str = "route";
const ACTIONABLE_ID: &str = "actionable";
const SEPARATE_THEMES_ID: &str = "separate_themes";
const COVERS_EVERYTHING_ID: &str = "covers_everything";
const STANDS_ALONE_PER_QUESTION_PREFIX: &str = "stands_alone_";
const STANDS_ALONE_MESSAGE_ID: &str = "stands_alone";
const EARLIER_TURNS_A_REWRITE_MAY_DRAW_ON: usize = 4;

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
const STANDS_ALONE_INSTRUCTIONS: &str = "Could this question be answered by someone shown only \
it, who had never seen the message it came from?";
const STANDS_ALONE_MESSAGE_INSTRUCTIONS: &str = "Could the new message be answered by someone \
shown only it, who had never seen the conversation it came from?";
const STANDS_ALONE_WHEN_TRUE: &str =
    "it names in full what it is about, and needs nothing else to be understood";
const STANDS_ALONE_WHEN_FALSE: &str = "it leans on something outside itself — \"there\", \"it\", \
\"that one\", \"the same\" — so alone nobody could tell what it asks about";

const RESOLVE_SYSTEM_PROMPT: &str = "\
The new message below leans on something said earlier — \"there\", \"it\", \"that one\", \"the \
same\". Rewrite it so it stands on its own.

Rules:
- Keep it the user's own request, first person, near-verbatim. Change only what the reference \
needs.
- Replace the reference with what the earlier turns say it is, taking those words from them and no \
further. \"what is the capital of Japan?\" then \"weather there?\" gives \"what is the weather in \
the capital of Japan?\" — never \"in Tokyo\", which nobody said. Working out what they meant is \
not your job, and working it out wrongly would be invisible from here.
- A message can ask about the earlier answer itself rather than about its subject — \"where?\" \
after a measurement asks which place it was for, not where that place is. Keep it a question about \
the earlier answer, naming that answer's own words.
- If the earlier turns do not say what the reference points at, return the message unchanged.
- Answer with the rewritten message and nothing else. No quotes, no explanation.
";

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
from the message and no further — never \"in Bishkek\", which the message does not say. Answering \
the other theme is not your job and guessing its answer wrongly would be invisible from here.
- Resolving a reference this way never consumes the theme it points at. That theme was asked for \
too and still gets its own topic. \"the capital of Kyrgyzstan, and the weather there\" is two \
topics — \"what is the capital of Kyrgyzstan?\" and \"what is the weather in the capital of \
Kyrgyzstan?\" — never the second one alone.
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
    Continue { topic_id: i64, question: String },
    New { title: String, question: String },
}

enum SplitVerdict {
    Keeps(Vec<Action>),
    MissingATheme,
    Unavailable,
}

const SPLIT_RETRY_NOTE: &str = "\nYour previous split was refused: between them, the questions did \
not ask for everything this message asks for. Something in it was left with no topic of its own. \
Split it again, covering all of it.\n";

pub enum Routing {
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

    fn route_options_cap(&self) -> Option<usize> {
        self.decision_budget
            .get()
            .and_then(|budget| usize::try_from(budget.max_choice_options).ok())
            .filter(|cap| *cap > 0)
    }

    pub async fn route(
        &self,
        topics: &[TopicSummary],
        focus: Option<i64>,
        message: &str,
    ) -> Routing {
        let Some(request) = build_request(topics, focus, message, self.route_options_cap()) else {
            return Routing::Act(continue_focus_or_open_one_topic(topics, focus, message));
        };

        let decided = match self.decider.decide(request).await {
            Ok(response) => response.into_owned(),
            Err(error) => {
                tracing::warn!(%error, "topic intent decision failed");
                return Routing::Act(continue_focus_or_open_one_topic(topics, focus, message));
            }
        };
        if decided.answers.is_empty() {
            return Routing::Act(continue_focus_or_open_one_topic(topics, focus, message));
        }

        if !topics.is_empty() {
            match route_outcome(&decided.answers, topics) {
                RouteOutcome::Continue(topic_id) => {
                    let question = self
                        .question_that_stands_alone(&decided.answers, topics, message)
                        .await;
                    return Routing::Act(vec![Action::Continue { topic_id, question }]);
                }
                RouteOutcome::Fallback => {
                    return Routing::Act(continue_focus_or_open_one_topic(topics, focus, message));
                }
                RouteOutcome::ConfidentNew => {}
            }
        }

        if noul_above(
            &decided.answers,
            SEPARATE_THEMES_ID,
            SPLIT_WHEN_SEPARATE_THEMES_ABOVE,
        ) && let Some(actions) = self.split(topics, message).await
        {
            return Routing::Act(actions);
        }

        if noul_below(
            &decided.answers,
            ACTIONABLE_ID,
            CLARIFY_WHEN_ACTIONABLE_BELOW,
        ) {
            return Routing::Clarify;
        }

        let question = self
            .question_that_stands_alone(&decided.answers, topics, message)
            .await;
        Routing::Act(vec![Action::New {
            title: truncate(message, MAX_TITLE_CHARS),
            question,
        }])
    }

    async fn question_that_stands_alone(
        &self,
        answers: &[Answer],
        topics: &[TopicSummary],
        message: &str,
    ) -> String {
        if leans_on_the_conversation(answers, STANDS_ALONE_MESSAGE_ID) {
            self.rewritten_against_the_conversation(topics, message)
                .await
        } else {
            message.to_owned()
        }
    }

    async fn rewritten_against_the_conversation(
        &self,
        topics: &[TopicSummary],
        message: &str,
    ) -> String {
        let Some(context) = recent_turns_newest_last(topics) else {
            return message.to_owned();
        };
        let response = self
            .llm
            .complete(CompleteRequest {
                tier: EnumValue::Known(QualityTier::Low),
                system_prompt: RESOLVE_SYSTEM_PROMPT.to_owned(),
                user_prompt: format!(
                    "Earlier in this conversation:\n{context}\n\nNew message: {message}\n"
                ),
                ..Default::default()
            })
            .await;
        match response {
            Ok(response) => {
                let resolved = response.into_owned().content.trim().to_owned();
                if resolved.is_empty() {
                    message.to_owned()
                } else {
                    resolved
                }
            }
            Err(error) => {
                tracing::warn!(%error, "could not resolve a message against the conversation");
                message.to_owned()
            }
        }
    }

    async fn split(&self, topics: &[TopicSummary], message: &str) -> Option<Vec<Action>> {
        let actions = self.ask_split(topics, message, "").await?;
        match self.judge_and_repair_split(message, actions).await {
            SplitVerdict::Keeps(actions) => Some(actions),
            SplitVerdict::Unavailable => None,
            SplitVerdict::MissingATheme => {
                tracing::info!("a split left a theme behind, asking once more");
                let again = self.ask_split(topics, message, SPLIT_RETRY_NOTE).await?;
                match self.judge_and_repair_split(message, again).await {
                    SplitVerdict::Keeps(actions) => Some(actions),
                    _ => None,
                }
            }
        }
    }

    async fn ask_split(
        &self,
        topics: &[TopicSummary],
        message: &str,
        retry_note: &str,
    ) -> Option<Vec<Action>> {
        let response = self
            .llm
            .complete(CompleteRequest {
                tier: EnumValue::Known(QualityTier::Low),
                system_prompt: SPLIT_SYSTEM_PROMPT.clone(),
                user_prompt: format!("New user message:\n{message}\n{retry_note}"),
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

    async fn judge_and_repair_split(&self, message: &str, actions: Vec<Action>) -> SplitVerdict {
        let questions: Vec<&str> = actions
            .iter()
            .filter_map(|action| match action {
                Action::New { question, .. } => Some(question.as_str()),
                Action::Continue { .. } => None,
            })
            .collect();
        let split_produced_no_questions = questions.is_empty();
        if split_produced_no_questions {
            return SplitVerdict::Keeps(actions);
        }
        let Some(asked) = split_judgement_questions(questions.len()) else {
            return SplitVerdict::Unavailable;
        };
        let state = serde_json::json!({ "message": message, "questions": questions });
        let Ok(base) =
            serde_json::from_value::<DecideRequest>(serde_json::json!({ "state": state }))
        else {
            return SplitVerdict::Unavailable;
        };
        let request = DecideRequest {
            questions: asked,
            ..base
        };
        let answers = match self.decider.decide(request).await {
            Ok(response) => response.into_owned().answers,
            Err(error) => {
                tracing::warn!(%error, "could not judge a split, so it was dropped");
                return SplitVerdict::Unavailable;
            }
        };
        if !noul_above(
            &answers,
            COVERS_EVERYTHING_ID,
            KEEP_SPLIT_WHEN_COVERS_EVERYTHING_ABOVE,
        ) {
            return SplitVerdict::MissingATheme;
        }
        SplitVerdict::Keeps(questions_made_to_stand_alone(actions, &answers, message))
    }
}

fn split_judgement_questions(question_count: usize) -> Option<Vec<Question>> {
    let mut asked = vec![covers_everything_question()?];
    for at in 0..question_count {
        asked.push(stands_alone_question(at)?);
    }
    Some(asked)
}

fn questions_made_to_stand_alone(
    actions: Vec<Action>,
    answers: &[Answer],
    message: &str,
) -> Vec<Action> {
    let mut at = 0;
    actions
        .into_iter()
        .map(|action| match action {
            Action::New { title, question } => {
                let id = format!("{STANDS_ALONE_PER_QUESTION_PREFIX}{at}");
                at += 1;
                let repaired = whole_message_scoped_to_title(message, &title);
                Action::New {
                    title,
                    question: if stands_alone_or_no_verdict_arrived(answers, &id) {
                        question
                    } else {
                        repaired
                    },
                }
            }
            other => other,
        })
        .collect()
}

fn whole_message_scoped_to_title(message: &str, title: &str) -> String {
    format!("{message}\n\nAnswer only this part of it: {title}")
}

fn stands_alone_question_for_message() -> Option<Question> {
    let mut question = instructed(
        STANDS_ALONE_MESSAGE_ID,
        serde_json::json!(STANDS_ALONE_MESSAGE_INSTRUCTIONS),
    )?;
    question.kind = Noul {
        when_true: Some(STANDS_ALONE_WHEN_TRUE.to_owned()),
        when_false: Some(STANDS_ALONE_WHEN_FALSE.to_owned()),
        ..Default::default()
    }
    .into();
    Some(question)
}

fn stands_alone_question(at: usize) -> Option<Question> {
    let instructions_kept_in_order = serde_json::json!([
        format!("About `questions[{at}]`, and nothing else:"),
        STANDS_ALONE_INSTRUCTIONS,
    ]);
    let mut question = instructed(
        &format!("{STANDS_ALONE_PER_QUESTION_PREFIX}{at}"),
        instructions_kept_in_order,
    )?;
    question.kind = Noul {
        when_true: Some(STANDS_ALONE_WHEN_TRUE.to_owned()),
        when_false: Some(STANDS_ALONE_WHEN_FALSE.to_owned()),
        ..Default::default()
    }
    .into();
    Some(question)
}

fn answered_without_talking_about_itself(topic: &TopicSummary) -> bool {
    topic.status == Status::Completed
        && topic
            .result_summary
            .as_deref()
            .is_some_and(|reply| !common::agent_replies::is_about_itself(reply))
}

fn recent_turns_newest_last(topics: &[TopicSummary]) -> Option<String> {
    let mut lines: Vec<String> = topics
        .iter()
        .rev()
        .take(EARLIER_TURNS_A_REWRITE_MAY_DRAW_ON)
        .map(|topic| {
            match topic
                .result_summary
                .as_deref()
                .filter(|_| answered_without_talking_about_itself(topic))
            {
                Some(answer) => format!("asked: {}\nanswered: {answer}", topic.title),
                None => format!("asked: {}", topic.title),
            }
        })
        .collect();
    if lines.is_empty() {
        return None;
    }
    lines.reverse();
    Some(lines.join("\n"))
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
    questions.push(stands_alone_question_for_message()?);

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

fn route_question(topics: &[TopicSummary], options_cap: Option<usize>) -> Option<Question> {
    let mut options: Vec<ChoiceOption> =
        most_recent_topics_leaving_room_for_new(topics, options_cap)
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

fn most_recent_topics_leaving_room_for_new(
    topics: &[TopicSummary],
    options_cap: Option<usize>,
) -> &[TopicSummary] {
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
    Continue(i64),
    Fallback,
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

fn route_outcome(answers: &[Answer], topics: &[TopicSummary]) -> RouteOutcome {
    let Some(choice) = find_choice(answers, ROUTE_ID) else {
        return RouteOutcome::Fallback;
    };
    if choice.confidence < CONTINUE_FOCUS_WHEN_ROUTE_CONFIDENCE_BELOW {
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

fn stands_alone_or_no_verdict_arrived(answers: &[Answer], id: &str) -> bool {
    find_noul(answers, id).is_none_or(|value| value > REWRITE_WHEN_STANDS_ALONE_BELOW)
}

fn leans_on_the_conversation(answers: &[Answer], id: &str) -> bool {
    find_noul(answers, id).is_some_and(|value| value < REWRITE_WHEN_STANDS_ALONE_BELOW)
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
                .then_some(Action::Continue {
                    topic_id,
                    question: String::new(),
                }),
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

#[must_use]
pub(crate) fn continue_focus_or_open_one_topic(
    topics: &[TopicSummary],
    focus: Option<i64>,
    message: &str,
) -> Vec<Action> {
    match focus {
        Some(topic_id) if topics.iter().any(|topic| topic.id == topic_id) => {
            vec![Action::Continue {
                topic_id,
                question: String::new(),
            }]
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

    const ACTIONABLE_LOW_ENOUGH_TO_CLARIFY: f64 = CLARIFY_WHEN_ACTIONABLE_BELOW - 0.1;

    fn topics() -> Vec<TopicSummary> {
        vec![TopicSummary {
            id: 7,
            title: "An earlier topic, still running".to_owned(),
            status: crate::entity::topic::Status::Running,
            result_summary: None,
        }]
    }

    #[tokio::test]
    async fn a_greeting_in_an_empty_session_asks_instead_of_starting_a_topic() {
        let (url, calls) =
            crate::fakes::serve_decider(vec![("actionable", ACTIONABLE_LOW_ENOUGH_TO_CLARIFY)])
                .await;
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
        let (url, _) = crate::fakes::serve_decider_choosing_with(
            "topic_7",
            0.9,
            vec![("actionable", ACTIONABLE_LOW_ENOUGH_TO_CLARIFY)],
        )
        .await;
        let intent = TopicIntent::new(&url).expect("client");
        let Routing::Act(actions) = intent.route(&topics(), Some(7), "and?").await else {
            panic!("a fragment aimed at a live topic must never clarify");
        };
        let [Action::Continue { topic_id, .. }] = actions.as_slice() else {
            panic!("expected one continuation: {actions:?}");
        };
        assert_eq!(*topic_id, 7);
    }

    #[tokio::test]
    async fn a_confident_route_continues_that_topic() {
        let (url, _) = crate::fakes::serve_decider_choosing("topic_7", 0.9).await;
        let intent = TopicIntent::new(&url).expect("client");
        let Routing::Act(actions) = intent.route(&topics(), Some(7), "and?").await else {
            panic!("expected actions");
        };
        let [Action::Continue { topic_id, .. }] = actions.as_slice() else {
            panic!("expected one continuation: {actions:?}");
        };
        assert_eq!(*topic_id, 7);
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
    async fn a_single_theme_message_leaning_on_the_conversation_is_resolved_against_it() {
        let (url, calls) = crate::fakes::serve_decider_resolving(
            0.08,
            "what is the weather in the capital of Japan?",
        )
        .await;
        let intent = TopicIntent::new(&url).expect("client");
        let topics = vec![TopicSummary {
            id: 7,
            title: "what is capital of Japan?".to_owned(),
            status: Status::Completed,
            result_summary: None,
        }];

        let Routing::Act(actions) = intent.route(&topics, None, "ok weather there?").await else {
            panic!("expected actions");
        };

        let [Action::New { question, title }] = actions.as_slice() else {
            panic!("expected one new topic: {actions:?}");
        };
        assert_eq!(question, "what is the weather in the capital of Japan?");
        assert_eq!(
            title, "ok weather there?",
            "the title stays the person's own words — only the question has to stand alone"
        );
        assert_eq!(
            calls.completions(),
            1,
            "and it cost one call, on the turn that needed it"
        );
    }

    #[tokio::test]
    async fn a_message_that_stands_alone_is_left_exactly_as_written() {
        let (url, calls) =
            crate::fakes::serve_decider_resolving(0.95, "something else entirely").await;
        let intent = TopicIntent::new(&url).expect("client");

        let Routing::Act(actions) = intent
            .route(&[], None, "what is the weather in Tokyo?")
            .await
        else {
            panic!("expected actions");
        };

        let [Action::New { question, .. }] = actions.as_slice() else {
            panic!("expected one new topic");
        };
        assert_eq!(question, "what is the weather in Tokyo?");
        assert_eq!(calls.completions(), 0);
    }

    #[tokio::test]
    async fn a_question_that_cannot_stand_alone_is_given_the_whole_message() {
        let (url, _calls) = crate::fakes::serve_decider_split_verdicts(
            0.9,
            0.95,
            0.05,
            r#"{"actions":[{"kind":"new","title":"A","question":"what is the capital?"},
                           {"kind":"new","title":"B","question":"what is the weather there?"}]}"#,
        )
        .await;
        let intent = TopicIntent::new(&url).expect("client");
        let message = "capital of Kyrgyzstan and what the weather there";

        let Routing::Act(actions) = intent.route(&[], None, message).await else {
            panic!("expected actions");
        };

        assert_eq!(actions.len(), 2, "the themes stay split: {actions:?}");
        for action in &actions {
            let Action::New { title, question } = action else {
                panic!("expected new topics");
            };
            assert!(
                question.starts_with(message),
                "the reference it leans on has to be there to resolve: {question:?}"
            );
            assert!(
                question.contains(&format!("Answer only this part of it: {title}")),
                "and it has to say which part is its own, or it answers all of them: {question:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_split_that_leaves_a_theme_behind_is_asked_for_again() {
        let (url, calls) = crate::fakes::serve_decider_split_asked_again(
            r#"{"actions":[{"kind":"new","title":"Weather","question":"what is the weather in the capital of Kyrgyzstan?"}]}"#,
            r#"{"actions":[{"kind":"new","title":"Capital","question":"what is the capital of Kyrgyzstan?"},
                           {"kind":"new","title":"Weather","question":"what is the weather in the capital of Kyrgyzstan?"}]}"#,
        )
        .await;
        let intent = TopicIntent::new(&url).expect("client");

        let Routing::Act(actions) = intent
            .route(
                &[],
                None,
                "capital of Kyrgyzstan and what the weather there",
            )
            .await
        else {
            panic!("expected actions");
        };

        assert_eq!(
            actions.len(),
            2,
            "the second ask's split is the one that is used: {actions:?}"
        );
        assert_eq!(calls.completions(), 2, "and it cost exactly one extra ask");
    }

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

    #[tokio::test(start_paused = true)]
    async fn a_decider_that_never_answers_is_abandoned_at_the_decide_deadline() {
        let url = crate::fakes::serve_decider_hanging().await;
        let intent = TopicIntent::new(&url).expect("client");

        let started = tokio::time::Instant::now();
        let Routing::Act(actions) = intent.route(&topics(), Some(7), "and?").await else {
            panic!("a hung decider must still fall back, not clarify");
        };
        let [Action::Continue { topic_id, .. }] = actions.as_slice() else {
            panic!("expected one continuation: {actions:?}");
        };
        assert_eq!(*topic_id, 7);

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
        let [Action::Continue { topic_id, .. }] = actions.as_slice() else {
            panic!("expected one continuation: {actions:?}");
        };
        assert_eq!(*topic_id, 7);
    }

    #[tokio::test]
    async fn a_topic_id_that_is_not_in_the_session_falls_back_to_the_focus() {
        let (url, _) = crate::fakes::serve_decider_choosing("topic_999", 0.9).await;
        let intent = TopicIntent::new(&url).expect("client");
        let Routing::Act(actions) = intent.route(&topics(), Some(7), "and?").await else {
            panic!("expected actions");
        };
        let [Action::Continue { topic_id, .. }] = actions.as_slice() else {
            panic!("expected one continuation: {actions:?}");
        };
        assert_eq!(*topic_id, 7);
    }

    #[tokio::test]
    async fn an_unsure_route_continues_the_focus() {
        let (url, _) = crate::fakes::serve_decider_choosing("new", 0.2).await;
        let intent = TopicIntent::new(&url).expect("client");
        let Routing::Act(actions) = intent.route(&topics(), Some(7), "and?").await else {
            panic!("expected actions");
        };
        let [Action::Continue { topic_id, .. }] = actions.as_slice() else {
            panic!("expected one continuation: {actions:?}");
        };
        assert_eq!(*topic_id, 7);
    }

    #[test]
    fn the_state_is_an_array_so_the_topic_order_survives() {
        let state = decision_state(&topics(), Some(7), "and?");
        assert!(
            state.is_array(),
            "an object's keys reach the model re-sorted: {state}"
        );
    }

    fn topics_oldest_first(count: i64) -> Vec<TopicSummary> {
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

    #[test]
    fn route_options_are_capped_at_the_budget_keeping_the_most_recent_plus_new() {
        let topics = topics_oldest_first(5);
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
        let topics = topics_oldest_first(5);
        let question = route_question(&topics, None).expect("a route question");
        assert_eq!(choice_option_names(&question).len(), 6, "5 topics plus new");
    }

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

        let topics = topics_oldest_first(5);
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

    #[tokio::test]
    async fn describe_models_being_unavailable_at_startup_leaves_chat_working_on_the_defaults() {
        let router = Arc::new(crate::fakes::FakeLlmRouter::default());
        router.answer_decision(Some(("new", 0.9)), vec![("actionable", 0.9)]);
        router.fail_describe_models();
        let url = crate::fakes::serve_router(Arc::clone(&router)).await;
        let intent = TopicIntent::new(&url).expect("client");

        intent.load_decision_budget().await;

        let topics = topics_oldest_first(5);
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

    #[tokio::test]
    async fn an_unreachable_router_at_startup_still_lets_a_turn_route_by_fallback() {
        let intent = TopicIntent::new("http://127.0.0.1:1").expect("client");

        intent.load_decision_budget().await;

        let Routing::Act(actions) = intent.route(&topics(), Some(7), "and?").await else {
            panic!("a failure must never clarify");
        };
        let [Action::Continue { topic_id, .. }] = actions.as_slice() else {
            panic!("expected one continuation: {actions:?}");
        };
        assert_eq!(*topic_id, 7);
    }
}
