// The public contract of the System One class: everything one Decide call may carry, and the check that
// turns a request over budget into a free local rejection rather than a paid provider 422.

use std::collections::HashSet;

use common::proto::llm_router::v1::{DecideRequest, DecisionBudget, question};
use connectrpc::ConnectError;
use serde_json::Value;

use crate::decision::{Decision, Question, QuestionKind};
use crate::wire::json_from;

pub const MAX_QUESTIONS: usize = 64;
// The vendor's own ceilings, published on its choice and score pages.
pub const MAX_CHOICE_OPTIONS: usize = 255;
pub const MAX_SCORE_LEVELS: usize = 10;
pub const MIN_SCORE_LEVELS: usize = 2;
pub const MAX_QUESTION_ID_BYTES: usize = 64;
pub const MAX_STATE_BYTES: usize = 262_144;
pub const MAX_INSTRUCTIONS_BYTES: usize = 8_192;
// The caller writes these and the provider bills for them, so each is bounded as well as counted: 255
// options of arbitrary length is a large paid request. An option name is the key its answer comes back
// under, exactly like a question id, and gets the same 64 bytes; a description, a score level and a noul's
// criteria are each a phrase or two, and get the byte limit one stop sequence already has.
pub const MAX_OPTION_NAME_BYTES: usize = MAX_QUESTION_ID_BYTES;
pub const MAX_OPTION_DESCRIPTION_BYTES: usize = 1_024;
pub const MAX_LEVEL_BYTES: usize = 1_024;

// The same numbers over the wire, so a caller in another language can check a request before sending it.
pub fn published() -> DecisionBudget {
    DecisionBudget {
        max_questions: counted(MAX_QUESTIONS),
        max_question_id_bytes: counted(MAX_QUESTION_ID_BYTES),
        max_instructions_bytes: counted(MAX_INSTRUCTIONS_BYTES),
        max_state_bytes: counted(MAX_STATE_BYTES),
        max_choice_options: counted(MAX_CHOICE_OPTIONS),
        max_option_name_bytes: counted(MAX_OPTION_NAME_BYTES),
        max_option_description_bytes: counted(MAX_OPTION_DESCRIPTION_BYTES),
        min_score_levels: counted(MIN_SCORE_LEVELS),
        max_score_levels: counted(MAX_SCORE_LEVELS),
        max_level_bytes: counted(MAX_LEVEL_BYTES),
        ..Default::default()
    }
}

pub fn validated_decision(request: DecideRequest) -> Result<Decision, ConnectError> {
    if request.questions.is_empty() {
        return Err(ConnectError::invalid_argument(
            "questions must hold at least one question",
        ));
    }
    if request.questions.len() > MAX_QUESTIONS {
        return Err(ConnectError::invalid_argument(format!(
            "questions must hold at most {MAX_QUESTIONS} questions",
        )));
    }

    let state = json_from(request.state.into_option())
        .map_err(|error| ConnectError::invalid_argument(format!("state {error}")))?;
    let state = within_json_budget("state", state, MAX_STATE_BYTES)?;

    let mut asked = HashSet::new();
    let mut questions = Vec::with_capacity(request.questions.len());
    for question in request.questions {
        validated_id(&question.id, &mut asked)?;
        questions.push(validated_question(question)?);
    }

    Ok(Decision { state, questions })
}

fn validated_id(id: &str, asked: &mut HashSet<String>) -> Result<(), ConnectError> {
    if id.is_empty() {
        return Err(ConnectError::invalid_argument("every question needs an id"));
    }
    if id.len() > MAX_QUESTION_ID_BYTES {
        return Err(ConnectError::invalid_argument(format!(
            "question id {id} is over the {MAX_QUESTION_ID_BYTES} byte limit"
        )));
    }
    if !asked.insert(id.to_owned()) {
        return Err(ConnectError::invalid_argument(format!(
            "question id {id} is asked twice"
        )));
    }
    Ok(())
}

fn validated_question(
    question: common::proto::llm_router::v1::Question,
) -> Result<Question, ConnectError> {
    let instructions = json_from(question.instructions.into_option()).map_err(|error| {
        ConnectError::invalid_argument(format!(
            "the instructions of question {} {error}",
            question.id
        ))
    })?;
    let instructions = within_json_budget(
        &format!("the instructions of question {}", question.id),
        instructions,
        MAX_INSTRUCTIONS_BYTES,
    )?;
    Ok(Question {
        instructions,
        kind: validated_kind(&question.id, question.kind)?,
        id: question.id,
    })
}

// A google.protobuf.Value cannot say "required", so this is the only place that can: a question or a state
// left unset arrives as JSON null, and asking a model about nothing still bills for the call.
fn within_json_budget(what: &str, value: Value, limit: usize) -> Result<Value, ConnectError> {
    if value.is_null() {
        return Err(ConnectError::invalid_argument(format!(
            "{what} must be set"
        )));
    }
    let bytes = value.to_string().len();
    if bytes > limit {
        return Err(ConnectError::invalid_argument(format!(
            "{what} is {bytes} bytes of JSON, over the {limit} byte limit"
        )));
    }
    Ok(value)
}

fn validated_kind(id: &str, kind: Option<question::Kind>) -> Result<QuestionKind, ConnectError> {
    match kind {
        None => Err(ConnectError::invalid_argument(format!(
            "question {id} names no kind"
        ))),
        Some(question::Kind::Noul(noul)) => Ok(QuestionKind::Noul {
            when_true: criterion(id, "the whenTrue criterion", noul.when_true)?,
            when_false: criterion(id, "the whenFalse criterion", noul.when_false)?,
        }),
        Some(question::Kind::Choice(choice)) => validated_options(id, choice.options),
        Some(question::Kind::Score(score)) => validated_levels(id, score.levels),
    }
}

fn criterion(
    id: &str,
    what: &str,
    written: Option<String>,
) -> Result<Option<String>, ConnectError> {
    written
        .map(|written| bounded(id, what, written, MAX_LEVEL_BYTES))
        .transpose()
}

fn bounded(id: &str, what: &str, written: String, limit: usize) -> Result<String, ConnectError> {
    if written.len() > limit {
        return Err(ConnectError::invalid_argument(format!(
            "{what} of question {id} is {} bytes, over the {limit} byte limit",
            written.len()
        )));
    }
    Ok(written)
}

fn validated_levels(id: &str, levels: Vec<String>) -> Result<QuestionKind, ConnectError> {
    if levels.len() < MIN_SCORE_LEVELS {
        return Err(ConnectError::invalid_argument(format!(
            "score question {id} needs at least {MIN_SCORE_LEVELS} levels"
        )));
    }
    if levels.len() > MAX_SCORE_LEVELS {
        return Err(ConnectError::invalid_argument(format!(
            "score question {id} names more than the {MAX_SCORE_LEVELS} levels this class takes"
        )));
    }
    let named = levels
        .into_iter()
        .enumerate()
        .map(|(index, level)| bounded(id, &format!("level {index}"), level, MAX_LEVEL_BYTES))
        .collect::<Result<Vec<String>, ConnectError>>()?;
    Ok(QuestionKind::Score { levels: named })
}

fn validated_options(
    id: &str,
    options: Vec<common::proto::llm_router::v1::ChoiceOption>,
) -> Result<QuestionKind, ConnectError> {
    if options.is_empty() {
        return Err(ConnectError::invalid_argument(format!(
            "choice question {id} names no options"
        )));
    }
    if options.len() > MAX_CHOICE_OPTIONS {
        return Err(ConnectError::invalid_argument(format!(
            "choice question {id} names more than the {MAX_CHOICE_OPTIONS} options this class takes"
        )));
    }

    let mut named = HashSet::new();
    let mut offered = Vec::with_capacity(options.len());
    for (index, mut option) in options.into_iter().enumerate() {
        if option.name.is_empty() {
            return Err(ConnectError::invalid_argument(format!(
                "every option of choice question {id} needs a name"
            )));
        }
        let what = format!("the name of option {index}");
        option.name = bounded(id, &what, option.name, MAX_OPTION_NAME_BYTES)?;
        if !named.insert(option.name.clone()) {
            return Err(ConnectError::invalid_argument(format!(
                "option {} of choice question {id} is offered twice",
                option.name
            )));
        }
        let what = format!("the description of option {}", option.name);
        option.description = option
            .description
            .map(|written| bounded(id, &what, written, MAX_OPTION_DESCRIPTION_BYTES))
            .transpose()?;
        offered.push(option);
    }

    Ok(QuestionKind::Choice { options: offered })
}

fn counted(limit: usize) -> i32 {
    i32::try_from(limit).unwrap_or(i32::MAX)
}

#[cfg(test)]
mod tests {
    use connectrpc::ErrorCode;
    use serde_json::json;

    use super::*;
    use crate::test_system_one::{
        choice_question, deciding, deciding_about, instructed, noul_question, score_question,
    };

    fn assert_refused(request: DecideRequest, offender: &str) {
        let error = validated_decision(request).unwrap_err();

        assert_eq!(error.code, ErrorCode::InvalidArgument, "{error:?}");
        assert!(format!("{error:?}").contains(offender), "{error:?}");
    }

    fn assert_question_refused(questions: Vec<common::proto::llm_router::v1::Question>, at: &str) {
        assert_refused(deciding(questions), at);
    }

    fn named(count: usize, prefix: &str) -> Vec<String> {
        (0..count).map(|at| format!("{prefix}{at}")).collect()
    }

    #[test]
    fn the_published_budget_is_the_budget_that_is_enforced() {
        let budget = published();

        assert_eq!(budget.max_questions, counted(MAX_QUESTIONS));
        assert_eq!(budget.max_choice_options, counted(MAX_CHOICE_OPTIONS));
        assert_eq!(budget.min_score_levels, counted(MIN_SCORE_LEVELS));
        assert_eq!(budget.max_score_levels, counted(MAX_SCORE_LEVELS));
        assert_eq!(budget.max_state_bytes, counted(MAX_STATE_BYTES));
        assert_eq!(
            budget.max_instructions_bytes,
            counted(MAX_INSTRUCTIONS_BYTES)
        );
        assert_eq!(budget.max_question_id_bytes, counted(MAX_QUESTION_ID_BYTES));
        assert_eq!(budget.max_option_name_bytes, counted(MAX_OPTION_NAME_BYTES));
        assert_eq!(
            budget.max_option_description_bytes,
            counted(MAX_OPTION_DESCRIPTION_BYTES)
        );
        assert_eq!(budget.max_level_bytes, counted(MAX_LEVEL_BYTES));
    }

    #[test]
    fn a_request_without_questions_is_refused() {
        assert_question_refused(Vec::new(), "questions");
    }

    #[test]
    fn more_questions_than_one_call_may_carry_are_refused() {
        let too_many = (0..=MAX_QUESTIONS)
            .map(|at| noul_question(&format!("question_{at}")))
            .collect();

        assert_question_refused(too_many, &MAX_QUESTIONS.to_string());
    }

    #[test]
    fn exactly_as_much_as_a_call_may_carry_is_accepted() {
        let names = named(MAX_CHOICE_OPTIONS, "o");
        let levels = named(MAX_SCORE_LEVELS, "level ");
        let mut asked: Vec<_> = (0..MAX_QUESTIONS - 2)
            .map(|at| noul_question(&format!("question_{at}")))
            .collect();
        asked.push(choice_question(
            "department",
            names
                .iter()
                .map(|name| (name.as_str(), "an option"))
                .collect(),
        ));
        asked.push(score_question(
            "frustration",
            &levels.iter().map(String::as_str).collect::<Vec<_>>(),
        ));

        let decision = validated_decision(deciding(asked)).unwrap();

        assert_eq!(decision.questions.len(), MAX_QUESTIONS);
        let QuestionKind::Choice { options } = &decision.questions[MAX_QUESTIONS - 2].kind else {
            panic!("a choice question keeps its options");
        };
        assert_eq!(options.len(), MAX_CHOICE_OPTIONS);
        let QuestionKind::Score { levels } = &decision.questions[MAX_QUESTIONS - 1].kind else {
            panic!("a score question keeps its levels");
        };
        assert_eq!(levels.len(), MAX_SCORE_LEVELS);
    }

    #[test]
    fn a_state_exactly_at_the_byte_limit_is_accepted() {
        // Two of the bytes are the quotes serde writes around the string.
        let at_the_limit = json!("x".repeat(MAX_STATE_BYTES - 2));

        validated_decision(deciding_about(
            at_the_limit,
            vec![noul_question("is_urgent")],
        ))
        .unwrap();
    }

    #[test]
    fn a_question_without_an_id_is_refused() {
        assert_question_refused(vec![noul_question("")], "id");
    }

    #[test]
    fn a_question_id_longer_than_the_limit_is_refused() {
        let long = "q".repeat(MAX_QUESTION_ID_BYTES + 1);

        assert_question_refused(
            vec![noul_question(&long)],
            &MAX_QUESTION_ID_BYTES.to_string(),
        );
    }

    #[test]
    fn a_question_id_asked_twice_is_refused() {
        assert_question_refused(
            vec![noul_question("is_urgent"), noul_question("is_urgent")],
            "is_urgent",
        );
    }

    #[test]
    fn a_state_larger_than_the_limit_is_refused() {
        let huge = json!("x".repeat(MAX_STATE_BYTES));

        assert_refused(
            deciding_about(huge, vec![noul_question("is_urgent")]),
            &MAX_STATE_BYTES.to_string(),
        );
    }

    #[test]
    fn a_state_the_caller_left_unset_is_refused_rather_than_asked_about() {
        assert_refused(
            deciding_about(json!(null), vec![noul_question("is_urgent")]),
            "state must be set",
        );
    }

    #[test]
    fn instructions_larger_than_the_limit_name_their_question() {
        let huge = json!("x".repeat(MAX_INSTRUCTIONS_BYTES));

        assert_question_refused(vec![instructed("is_urgent", huge)], "is_urgent");
    }

    #[test]
    fn instructions_the_caller_left_unset_are_refused_rather_than_asked_about() {
        assert_question_refused(vec![instructed("is_urgent", json!(null))], "must be set");
    }

    #[test]
    fn a_question_without_a_kind_is_refused() {
        let mut bare = noul_question("is_urgent");
        bare.kind = None;

        assert_question_refused(vec![bare], "kind");
    }

    #[test]
    fn a_choice_without_options_is_refused() {
        assert_question_refused(vec![choice_question("department", Vec::new())], "options");
    }

    #[test]
    fn a_choice_with_more_options_than_the_class_takes_is_refused() {
        let names = named(MAX_CHOICE_OPTIONS + 1, "o");
        let too_many = names
            .iter()
            .map(|name| (name.as_str(), "an option"))
            .collect();

        assert_question_refused(
            vec![choice_question("department", too_many)],
            &MAX_CHOICE_OPTIONS.to_string(),
        );
    }

    #[test]
    fn a_choice_option_without_a_name_is_refused() {
        assert_question_refused(
            vec![choice_question("department", vec![("", "Payments")])],
            "name",
        );
    }

    #[test]
    fn a_choice_option_offered_twice_is_refused() {
        assert_question_refused(
            vec![choice_question(
                "department",
                vec![("billing", "Payments"), ("billing", "Refunds")],
            )],
            "billing",
        );
    }

    #[test]
    fn an_option_name_or_description_over_its_byte_limit_is_refused() {
        let long_name = "b".repeat(MAX_OPTION_NAME_BYTES + 1);
        let long_description = "p".repeat(MAX_OPTION_DESCRIPTION_BYTES + 1);

        assert_question_refused(
            vec![choice_question(
                "department",
                vec![(long_name.as_str(), "Payments")],
            )],
            &MAX_OPTION_NAME_BYTES.to_string(),
        );
        assert_question_refused(
            vec![choice_question(
                "department",
                vec![("billing", long_description.as_str())],
            )],
            &MAX_OPTION_DESCRIPTION_BYTES.to_string(),
        );
    }

    #[test]
    fn a_score_with_a_single_level_is_refused() {
        assert_question_refused(vec![score_question("frustration", &["Calm"])], "levels");
    }

    #[test]
    fn a_score_with_more_levels_than_the_class_takes_is_refused() {
        let levels = named(MAX_SCORE_LEVELS + 1, "level ");
        let too_many: Vec<&str> = levels.iter().map(String::as_str).collect();

        assert_question_refused(
            vec![score_question("frustration", &too_many)],
            &MAX_SCORE_LEVELS.to_string(),
        );
    }

    #[test]
    fn a_score_level_over_its_byte_limit_is_refused() {
        let long = "x".repeat(MAX_LEVEL_BYTES + 1);

        assert_question_refused(
            vec![score_question("frustration", &["Calm", long.as_str()])],
            &MAX_LEVEL_BYTES.to_string(),
        );
    }

    #[test]
    fn a_noul_criterion_over_its_byte_limit_is_refused() {
        let long = "x".repeat(MAX_LEVEL_BYTES + 1);
        let mut question = noul_question("is_urgent");
        question.kind = common::proto::llm_router::v1::Noul {
            when_true: Some(long),
            ..Default::default()
        }
        .into();

        assert_question_refused(vec![question], &MAX_LEVEL_BYTES.to_string());
    }
}
