// Routes one user turn onto existing or new topics with a low-tier llm-router call.

use std::sync::LazyLock;
use std::time::Duration;

use buffa::EnumValue;
use common::proto::llm_router::v1::{
    CompleteRequest, LlmRouterServiceClient, QualityTier, ResponseFormat, Sampling,
};
use connectrpc::Protocol;
use connectrpc::client::{ClientConfig, HttpClient};
use serde::{Deserialize, Serialize};

use crate::entity::topic::Status;
use crate::topic_status::status_word;

const CALL_TIMEOUT: Duration = Duration::from_secs(30);
pub const MAX_TITLE_CHARS: usize = 60;
const MAX_SUMMARY_PREVIEW_CHARS: usize = 160;

const EXAMPLE_TOPIC_ID: i64 = 7;
const EXAMPLE_NEW_TOPIC_ID: i64 = 123;
const ELLIPSIS: &str = "…";

static SYSTEM_PROMPT: LazyLock<String> = LazyLock::new(|| {
    format!(
        "{PROMPT_RULES}- `title` is a short label (at most {MAX_TITLE_CHARS} characters) for the \
         topic list.\n{}",
        *PROMPT_EXAMPLES
    )
});

const PROMPT_RULES: &str = "\
You route a user's chat message to topics. A session is a set of topics; each topic is an \
independent task being worked on in the background. Your job is to decide, for the new message, \
which existing topics it continues and which new topics it opens.

Rules:
- A follow-up, complaint, clarification, or nudge about the same subject is ALWAYS `continue` on \
that topic — this includes things like \"i'm still waiting\", \"and?\", \"more details please\", \
\"that's not what I asked\", or a rephrasing of the same request. Never turn these into a `new` \
topic, and never turn them into a meta-instruction about \"the user\" — they continue the \
existing topic exactly as the classifier is asked to route them.
- A message that adds a new, unrelated request — \"also tell me about X\" where X is a different \
subject — opens a new topic for X (and continues the existing topic for the rest of the message, \
if any).
- A first message that names several separate themes opens one new topic per theme.
- Otherwise the message continues the focused topic, or another existing topic it clearly \
matches.
- When in doubt, continue the focused topic. Only create a topic for a genuinely separate task.
- For every new topic, `question` must be the user's OWN request, first person, near-verbatim — \
copy their wording. Only minimally expand it with necessary context that is missing from the \
message itself (e.g. a place or name mentioned earlier in the session). Never write a \
paraphrased instruction about \"the user\" (e.g. never \"the user asked about X, please answer \
it\") — `question` is what the user themselves would have typed, not a description of their \
request.
";

static PROMPT_EXAMPLES: LazyLock<String> = LazyLock::new(|| {
    let continue_focus = || RawAction::Continue {
        topic_id: EXAMPLE_TOPIC_ID,
    };
    let new_topic = |title: &str, question: &str| RawAction::New {
        title: Some(title.to_owned()),
        question: Some(question.to_owned()),
    };
    format!(
        "
Examples (existing topic id={EXAMPLE_TOPIC_ID}, title \"Weather in San Francisco today\", focused):

User: \"i'm still waiting\"
{}

User: \"and?\"
{}

User: \"also, what is Rust's latest stable version?\"
{}

User (first message of the session, no existing topics): \"What is Claude Code? And which \
Academy courses exist?\"
{}

Answer with strict JSON and nothing else, in exactly this shape:
{}
At least one action. Several actions are allowed.",
        plan_shape(vec![continue_focus()]),
        plan_shape(vec![continue_focus()]),
        plan_shape(vec![
            continue_focus(),
            new_topic(
                "Rust's latest stable version",
                "What is Rust's latest stable version?"
            ),
        ]),
        plan_shape(vec![
            new_topic("Claude Code", "What is Claude Code?"),
            new_topic("Academy courses", "Which Academy courses exist?"),
        ]),
        plan_shape(vec![
            RawAction::Continue {
                topic_id: EXAMPLE_NEW_TOPIC_ID,
            },
            new_topic(ELLIPSIS, ELLIPSIS),
        ]),
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

pub struct TopicClassifier {
    client: LlmRouterServiceClient<HttpClient>,
}

impl TopicClassifier {
    pub fn new(llm_router_url: &str) -> Result<Self, String> {
        let target = llm_router_url
            .parse()
            .map_err(|e| format!("could not parse LLM_ROUTER_URL {llm_router_url:?}: {e}"))?;
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

    pub async fn classify(
        &self,
        topics: &[TopicSummary],
        focus: Option<i64>,
        message: &str,
    ) -> Vec<Action> {
        match self.ask(topics, focus, message).await {
            Ok(content) => match parse_actions(&content, topics) {
                Some(actions) => actions,
                None => {
                    tracing::warn!(%content, "topic classifier returned unusable output");
                    fallback(topics, focus, message)
                }
            },
            Err(error) => {
                tracing::warn!(%error, "topic classifier call failed");
                fallback(topics, focus, message)
            }
        }
    }

    async fn ask(
        &self,
        topics: &[TopicSummary],
        focus: Option<i64>,
        message: &str,
    ) -> Result<String, String> {
        let response = self
            .client
            .complete(CompleteRequest {
                tier: EnumValue::Known(QualityTier::Low),
                system_prompt: SYSTEM_PROMPT.clone(),
                user_prompt: user_prompt(topics, focus, message),
                sampling: Sampling {
                    response_format: Some(EnumValue::Known(ResponseFormat::JsonObject)),
                    ..Default::default()
                }
                .into(),
                ..Default::default()
            })
            .await
            .map_err(|error| error.to_string())?;
        Ok(response.into_owned().content)
    }
}

#[must_use]
fn user_prompt(topics: &[TopicSummary], focus: Option<i64>, message: &str) -> String {
    let mut prompt = String::from("Existing topics in this session:\n");
    if topics.is_empty() {
        prompt.push_str("(none — this is the first message of the session)\n");
    }
    for topic in topics {
        let result = topic
            .result_summary
            .as_deref()
            .map(one_line)
            .unwrap_or_default();
        prompt.push_str(&format!(
            "- id={} status={} title={:?} result={:?}\n",
            topic.id,
            status_word(topic.status),
            topic.title,
            result
        ));
    }
    prompt.push_str(&match focus {
        Some(id) => format!("\nFocused topic id: {id}\n"),
        None => "\nFocused topic id: none\n".to_owned(),
    });
    prompt.push_str(&format!("\nNew user message:\n{message}\n"));
    prompt
}

fn one_line(text: &str) -> String {
    let first = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("");
    first.chars().take(MAX_SUMMARY_PREVIEW_CHARS).collect()
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

#[must_use]
fn fallback(topics: &[TopicSummary], focus: Option<i64>, message: &str) -> Vec<Action> {
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

    fn topics() -> Vec<TopicSummary> {
        vec![TopicSummary {
            id: 7,
            title: "Claude Code".to_owned(),
            status: Status::Running,
            result_summary: None,
        }]
    }

    #[test]
    fn a_fenced_answer_with_trailing_prose_still_parses() {
        let content = "```json\n{\"actions\":[{\"kind\":\"continue\",\"topic_id\":7}]}\n```\nHope that helps!";
        assert_eq!(
            parse_actions(content, &topics()),
            Some(vec![Action::Continue { topic_id: 7 }])
        );
    }

    #[test]
    fn several_new_actions_are_kept_in_order_and_titles_are_bounded() {
        let long = "x".repeat(200);
        let content = format!(
            "{{\"actions\":[{{\"kind\":\"new\",\"title\":\"{long}\",\"question\":\"q1\"}},\
             {{\"kind\":\"new\",\"title\":\"B\",\"question\":\"q2\"}}]}}"
        );
        let actions = parse_actions(&content, &[]).expect("parsed");
        assert_eq!(actions.len(), 2);
        let Action::New { title, question } = &actions[0] else {
            panic!("expected a new-topic action");
        };
        assert_eq!(title.chars().count(), MAX_TITLE_CHARS);
        assert_eq!(question, "q1");
    }

    #[test]
    fn a_continue_for_a_topic_that_is_not_in_the_session_is_dropped() {
        assert_eq!(
            parse_actions(
                "{\"actions\":[{\"kind\":\"continue\",\"topic_id\":999}]}",
                &topics()
            ),
            None,
            "a hallucinated topic id must not route the turn anywhere"
        );
    }

    #[test]
    fn unusable_output_is_rejected_so_the_caller_can_fall_back() {
        for content in ["", "sorry, I can't", "{\"actions\":[]}", "{not json}"] {
            assert_eq!(parse_actions(content, &topics()), None, "{content:?}");
        }
    }

    #[test]
    fn the_fallback_opens_a_topic_for_an_empty_session_and_continues_focus_otherwise() {
        assert_eq!(
            fallback(&[], None, "what is Claude Code?"),
            vec![Action::New {
                title: "what is Claude Code?".to_owned(),
                question: "what is Claude Code?".to_owned(),
            }]
        );
        assert_eq!(
            fallback(&topics(), Some(7), "and IDEs?"),
            vec![Action::Continue { topic_id: 7 }]
        );
        let focus_on_a_vanished_topic = Some(99);
        assert!(matches!(
            fallback(&topics(), focus_on_a_vanished_topic, "hi").as_slice(),
            [Action::New { .. }]
        ));
    }

    #[test]
    fn the_system_prompt_biases_toward_continuing_the_focus_topic() {
        assert!(
            SYSTEM_PROMPT.contains("i'm still waiting"),
            "must give a still-waiting/follow-up example"
        );
        assert!(
            SYSTEM_PROMPT.contains("ALWAYS `continue`"),
            "must state the continue-on-follow-up rule explicitly"
        );
        assert!(
            SYSTEM_PROMPT.contains("When in doubt, continue the focused topic"),
            "must state the bias toward continuing"
        );
    }

    #[test]
    fn the_system_prompt_requires_first_person_near_verbatim_questions() {
        assert!(
            SYSTEM_PROMPT.contains("first person, near-verbatim"),
            "must require the user's own wording for a new topic's question"
        );
        assert!(
            SYSTEM_PROMPT.contains("Never write a paraphrased instruction about \"the user\""),
            "must forbid third-person meta-instructions as the question"
        );
    }

    #[test]
    fn the_system_prompt_has_few_shot_examples_for_each_rule() {
        assert!(SYSTEM_PROMPT.contains("\"actions\":[{\"kind\":\"continue\",\"topic_id\":7}]"));
        assert!(SYSTEM_PROMPT.contains("also, what is Rust's latest stable version?"));
        assert!(SYSTEM_PROMPT.contains("Rust's latest stable version"));
        assert!(SYSTEM_PROMPT.contains("Academy courses"));
    }

    #[test]
    fn the_prompt_carries_the_topics_the_focus_and_the_message() {
        let prompt = user_prompt(&topics(), Some(7), "and IDEs?");
        assert!(prompt.contains("id=7"));
        assert!(prompt.contains("status=running"));
        assert!(prompt.contains("Claude Code"));
        assert!(prompt.contains("Focused topic id: 7"));
        assert!(prompt.contains("and IDEs?"));
    }

    #[test]
    fn the_system_prompt_states_the_real_title_limit() {
        assert!(
            SYSTEM_PROMPT.contains(&format!("at most {MAX_TITLE_CHARS} characters")),
            "the prompt must quote the constant the parser actually enforces"
        );
    }

    #[test]
    fn a_summary_preview_is_one_bounded_line() {
        let long = format!("{}\nsecond line", "x".repeat(400));
        let preview = one_line(&long);
        assert_eq!(preview.chars().count(), MAX_SUMMARY_PREVIEW_CHARS);
    }
}
