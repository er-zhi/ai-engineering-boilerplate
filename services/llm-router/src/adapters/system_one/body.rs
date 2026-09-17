// The two renderings of one decision: the object the provider's API takes, and the array the audit log keeps.

use serde::Serialize;
use serde::ser::{SerializeMap, Serializer};
use serde_json::{Map, Value, json};

use common::proto::llm_router::v1::ChoiceOption;

use crate::decision::{Decision, Question, QuestionKind};

// The body the provider is sent, serialized straight from the caller's questions so their order survives to the model.
#[derive(Serialize)]
pub struct DecideBody<'a> {
    state: &'a Value,
    model: &'a str,
    questions: AskedQuestions<'a>,
}

struct AskedQuestions<'a>(&'a [Question]);

struct AskedQuestion<'a>(&'a Question);

struct ChoiceCriteria<'a>(&'a [ChoiceOption]);

impl Serialize for AskedQuestions<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut questions = serializer.serialize_map(Some(self.0.len()))?;
        for question in self.0 {
            questions.serialize_entry(&question.id, &AskedQuestion(question))?;
        }
        questions.end()
    }
}

impl Serialize for AskedQuestion<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut question = serializer.serialize_map(None)?;
        question.serialize_entry("type", type_name(&self.0.kind))?;
        question.serialize_entry("instructions", &self.0.instructions)?;
        match &self.0.kind {
            QuestionKind::Noul {
                when_true,
                when_false,
            } => {
                let criteria = noul_criteria(when_true.as_deref(), when_false.as_deref());
                if !criteria.is_empty() {
                    question.serialize_entry("criteria", &criteria)?;
                }
            }
            QuestionKind::Choice { options } => {
                question.serialize_entry("criteria", &ChoiceCriteria(options))?;
            }
            QuestionKind::Score { levels } => question.serialize_entry("criteria", levels)?,
        }
        question.end()
    }
}

impl Serialize for ChoiceCriteria<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut criteria = serializer.serialize_map(Some(self.0.len()))?;
        for option in self.0 {
            criteria.serialize_entry(&option.name, &option.description)?;
        }
        criteria.end()
    }
}

pub fn decide_body<'a>(model: &'a str, decision: &'a Decision) -> DecideBody<'a> {
    DecideBody {
        state: &decision.state,
        model,
        questions: AskedQuestions(&decision.questions),
    }
}

// The same request for the decision log. The questions are an array here: jsonb has no key order of its own,
// and the order the caller asked in is the whole reason the contract takes them as a list.
pub fn audit_body(model: &str, decision: &Decision) -> Value {
    json!({
        "model": model,
        "state": decision.state,
        "questions": decision.questions.iter().map(audited_question).collect::<Vec<Value>>(),
    })
}

fn audited_question(question: &Question) -> Value {
    let mut asked = Map::new();
    asked.insert("id".to_owned(), json!(question.id));
    asked.insert("type".to_owned(), json!(type_name(&question.kind)));
    asked.insert("instructions".to_owned(), question.instructions.clone());
    match &question.kind {
        QuestionKind::Noul {
            when_true,
            when_false,
        } => {
            let criteria = noul_criteria(when_true.as_deref(), when_false.as_deref());
            if !criteria.is_empty() {
                asked.insert("criteria".to_owned(), Value::Object(criteria));
            }
        }
        QuestionKind::Choice { options } => {
            let offered = options
                .iter()
                .map(|option| json!({"name": option.name, "description": option.description}))
                .collect::<Vec<Value>>();
            asked.insert("criteria".to_owned(), Value::Array(offered));
        }
        QuestionKind::Score { levels } => {
            asked.insert("criteria".to_owned(), json!(levels));
        }
    }
    Value::Object(asked)
}

fn noul_criteria(when_true: Option<&str>, when_false: Option<&str>) -> Map<String, Value> {
    let mut criteria = Map::new();
    if let Some(when_true) = when_true {
        criteria.insert("true".to_owned(), json!(when_true));
    }
    if let Some(when_false) = when_false {
        criteria.insert("false".to_owned(), json!(when_false));
    }
    criteria
}

pub fn type_name(kind: &QuestionKind) -> &'static str {
    match kind {
        QuestionKind::Noul { .. } => "noul",
        QuestionKind::Choice { .. } => "choice",
        QuestionKind::Score { .. } => "score",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_system_one::{decision, department, urgency};

    const MODEL: &str = "jev-latest";

    #[test]
    fn choice_options_reach_the_model_in_the_order_the_caller_offered_them() {
        let body =
            serde_json::to_string(&decide_body(MODEL, &decision(vec![department()]))).unwrap();

        let billing = body.find("\"billing\"").unwrap();
        let technical = body.find("\"technical\"").unwrap();
        let sales = body.find("\"sales\"").unwrap();
        assert!(billing < technical && technical < sales, "{body}");
    }

    #[test]
    fn an_option_without_a_description_is_offered_as_null() {
        let body =
            serde_json::to_string(&decide_body(MODEL, &decision(vec![department()]))).unwrap();

        assert!(body.contains("\"sales\":null"), "{body}");
    }

    #[test]
    fn a_noul_without_criteria_sends_none_at_all() {
        let bare = Question {
            kind: QuestionKind::Noul {
                when_true: None,
                when_false: None,
            },
            ..urgency()
        };

        let body =
            serde_json::to_string(&decide_body(MODEL, &decision(vec![bare.clone()]))).unwrap();

        assert!(!body.contains("criteria"), "{body}");
        assert_eq!(
            audit_body(MODEL, &decision(vec![bare]))["questions"][0].get("criteria"),
            None
        );
    }

    #[test]
    fn the_logged_request_keeps_the_questions_in_the_order_they_were_asked() {
        let logged = audit_body(
            MODEL,
            &decision(vec![
                crate::test_system_one::frustration(),
                urgency(),
                department(),
            ]),
        );

        let asked: Vec<&str> = logged["questions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|question| question["id"].as_str().unwrap())
            .collect();
        assert_eq!(asked, ["frustration", "is_urgent", "department"]);
        let offered: Vec<&str> = logged["questions"][2]["criteria"]
            .as_array()
            .unwrap()
            .iter()
            .map(|option| option["name"].as_str().unwrap())
            .collect();
        assert_eq!(offered, ["billing", "technical", "sales"]);
    }
}
