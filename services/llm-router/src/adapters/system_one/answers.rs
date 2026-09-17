// Reads a System One reply into the typed answers the caller asked for, checking every value against the
// question that produced it: the provider's payload is a claim about the caller's own scale, not a fact.

use common::proto::llm_router::v1::{
    Answer, ChoiceAnswer, ChoiceOption, ModelCard, NoulAnswer, ScoreAnswer,
};
use serde_json::Value;

use super::body::type_name;
use super::text;
use crate::adapters::as_count;
use crate::decision::{Question, QuestionKind, Verdict};
use crate::provider::CallError;

pub fn read_verdict(
    body: &Value,
    questions: &[Question],
    sent: &Value,
) -> Result<Verdict, CallError> {
    let Some(answers) = body["answers"].as_object() else {
        return Err(CallError::WorthRetrying(
            "the reply carried no answers".to_owned(),
        ));
    };
    if let Some(unknown) = answers
        .keys()
        .find(|id| !questions.iter().any(|question| &&question.id == id))
    {
        return Err(CallError::Final(format!(
            "the provider answered {unknown}, which was never asked"
        )));
    }

    let mut given = Vec::with_capacity(questions.len());
    for question in questions {
        let Some(answer) = answers.get(&question.id) else {
            return Err(CallError::Final(format!(
                "the provider left {} unanswered",
                question.id
            )));
        };
        given.push(read_answer(question, answer)?);
    }

    Ok(Verdict {
        model_used: text(&body["model"]),
        answers: given,
        tokens_in: as_count("input_tokens", &body["usage"]["input_tokens"]),
        tokens_out: as_count("output_tokens", &body["usage"]["output_tokens"]),
        sent: sent.clone(),
        reply: body.clone(),
    })
}

pub fn model_card(model: &Value) -> ModelCard {
    ModelCard {
        name: text(&model["name"]),
        description: text(&model["description"]),
        release_date: text(&model["release_date"]),
        ..Default::default()
    }
}

// The decision values themselves are read as strictly as the answer's shape: a missing noul defaulting to
// zero is a confident no the model never gave, and a missing score is the lowest level it never chose. The
// extras around them — confidence, probabilities — are optional, and an unset optional serialized as JSON
// null means the same as a key the provider left out.
fn read_answer(question: &Question, answer: &Value) -> Result<Answer, CallError> {
    let id = question.id.as_str();
    let reported = answer["type"].as_str().unwrap_or_default();
    let given = match (&question.kind, reported) {
        (QuestionKind::Noul { .. }, "noul") => NoulAnswer {
            noul: probability(id, "noul", &answer["noul"])?,
            ..Default::default()
        }
        .into(),
        (QuestionKind::Choice { options }, "choice") => ChoiceAnswer {
            choice: chosen_option(id, &answer["choice"], options)?,
            probabilities: named_probabilities(id, given_value(answer, "probabilities"), options)?
                .into_iter()
                .collect(),
            confidence: confidence(id, given_value(answer, "confidence"))?,
            ..Default::default()
        }
        .into(),
        (QuestionKind::Score { levels }, "score") => ScoreAnswer {
            score: scored(id, &answer["score"], levels.len())?,
            legend: caller_legend(levels).into_iter().collect(),
            probabilities: levelled_probabilities(
                id,
                given_value(answer, "probabilities"),
                levels.len(),
            )?
            .into_iter()
            .collect(),
            confidence: confidence(id, given_value(answer, "confidence"))?,
            ..Default::default()
        }
        .into(),
        _ => {
            return Err(CallError::Final(format!(
                "the provider answered {id} with a {reported}, not the {} it was asked",
                type_name(&question.kind)
            )));
        }
    };

    Ok(Answer {
        id: id.to_owned(),
        answer: given,
        ..Default::default()
    })
}

// A provider that serializes an unset optional as null rather than omitting the key means the same thing.
fn given_value<'a>(answer: &'a Value, field: &str) -> Option<&'a Value> {
    answer.get(field).filter(|value| !value.is_null())
}

// The legend is the caller's own scale by index rather than the provider's echo of it: validating only the
// index would let a paraphrase come back as the levels the caller wrote, two copies of one declaration with
// nothing keeping them in step.
fn caller_legend(levels: &[String]) -> Vec<(u32, String)> {
    levels
        .iter()
        .enumerate()
        .map(|(index, level)| (u32::try_from(index).unwrap_or(u32::MAX), level.clone()))
        .collect()
}

fn chosen_option(id: &str, value: &Value, options: &[ChoiceOption]) -> Result<String, CallError> {
    let Some(chosen) = value.as_str() else {
        return Err(CallError::Final(format!(
            "the provider answered {id} with no readable choice"
        )));
    };
    if !offered(options, chosen) {
        return Err(CallError::Final(format!(
            "the provider answered {id} with {chosen}, which was never offered"
        )));
    }
    Ok(chosen.to_owned())
}

fn offered(options: &[ChoiceOption], name: &str) -> bool {
    options.iter().any(|option| option.name == name)
}

fn levelled_probabilities(
    id: &str,
    value: Option<&Value>,
    levels: usize,
) -> Result<Vec<(u32, f64)>, CallError> {
    let Some(entries) = object(id, "probabilities", value)? else {
        return Ok(Vec::new());
    };
    entries
        .iter()
        .map(|(level, weight)| {
            let index: u32 = level.parse().map_err(|_| {
                CallError::Final(format!(
                    "the provider answered {id} with a level {level} that is not a number"
                ))
            })?;
            if index as usize >= levels {
                return Err(CallError::Final(format!(
                    "the provider answered {id} with a level {index}, past the {levels} levels it \
                     was asked about"
                )));
            }
            Ok((index, probability(id, "probability", weight)?))
        })
        .collect()
}

fn named_probabilities(
    id: &str,
    value: Option<&Value>,
    options: &[ChoiceOption],
) -> Result<Vec<(String, f64)>, CallError> {
    let Some(weights) = object(id, "probabilities", value)? else {
        return Ok(Vec::new());
    };
    weights
        .iter()
        .map(|(name, weight)| {
            if !offered(options, name) {
                return Err(CallError::Final(format!(
                    "the provider answered {id} with a probability for {name}, which was never \
                     offered"
                )));
            }
            Ok((name.clone(), probability(id, "probability", weight)?))
        })
        .collect()
}

fn object<'a>(
    id: &str,
    field: &str,
    value: Option<&'a Value>,
) -> Result<Option<&'a serde_json::Map<String, Value>>, CallError> {
    let Some(value) = value else {
        return Ok(None);
    };
    value.as_object().map(Some).ok_or_else(|| {
        CallError::Final(format!(
            "the provider answered {id} with a {field} that is not an object"
        ))
    })
}

fn number(id: &str, field: &str, value: &Value) -> Result<f64, CallError> {
    value.as_f64().ok_or_else(|| {
        CallError::Final(format!(
            "the provider answered {id} with no readable {field}"
        ))
    })
}

fn probability(id: &str, field: &str, value: &Value) -> Result<f64, CallError> {
    let read = number(id, field, value)?;
    if !(0.0..=1.0).contains(&read) {
        return Err(CallError::Final(format!(
            "the provider answered {id} with a {field} of {read}, which is not between 0 and 1"
        )));
    }
    Ok(read)
}

// A score is a weighted position on the caller's own scale, so it lives between 0 and one less than the
// number of levels: 47 on a five-level scale is not a rating the caller can read back.
fn scored(id: &str, value: &Value, levels: usize) -> Result<f64, CallError> {
    let read = number(id, "score", value)?;
    let highest = f64::from(u32::try_from(levels.saturating_sub(1)).unwrap_or(u32::MAX));
    if !(0.0..=highest).contains(&read) {
        return Err(CallError::Final(format!(
            "the provider answered {id} with a score of {read}, outside the 0 to {highest} scale of \
             the {levels} levels it was asked about"
        )));
    }
    Ok(read)
}

// A confidence the provider left out fails safe at zero; one it sent but we cannot read does not.
fn confidence(id: &str, value: Option<&Value>) -> Result<f64, CallError> {
    match value {
        None => Ok(0.0),
        Some(value) => probability(id, "confidence", value),
    }
}

#[cfg(test)]
mod tests {
    use common::proto::llm_router::v1::answer::Answer as Given;
    use serde_json::json;

    use super::*;
    use crate::test_system_one::{answered_reply, department, frustration, scored_reply, urgency};

    fn final_message(error: CallError) -> String {
        let CallError::Final(message) = error else {
            panic!("a reply this adapter cannot make sense of is a contract fault, not a blip");
        };
        message
    }

    fn refused(reply: &Value, questions: &[Question]) -> String {
        final_message(read_verdict(reply, questions, &json!({})).unwrap_err())
    }

    fn answered_ids(verdict: &Verdict) -> Vec<&str> {
        verdict
            .answers
            .iter()
            .map(|answer| answer.id.as_str())
            .collect()
    }

    #[test]
    fn answers_come_back_in_the_order_the_caller_asked() {
        let questions = vec![frustration(), urgency(), department()];

        let verdict = read_verdict(&answered_reply(), &questions, &json!({})).unwrap();

        assert_eq!(
            answered_ids(&verdict),
            ["frustration", "is_urgent", "department"]
        );
        assert_eq!(verdict.model_used, "jev-1.13.0");
        assert_eq!(verdict.tokens_in, 312);
        assert_eq!(verdict.tokens_out, 48);
        let Some(Given::Score(score)) = &verdict.answers[0].answer else {
            panic!("a score question is answered with a score");
        };
        assert_eq!(score.score, 1.6);
        assert_eq!(score.legend[&2], "Very angry");
        assert_eq!(score.probabilities[&2], 0.6);
        let Some(Given::Choice(choice)) = &verdict.answers[2].answer else {
            panic!("a choice question is answered with a choice");
        };
        assert_eq!(choice.choice, "billing");
        assert_eq!(choice.probabilities["billing"], 0.81);
        assert_eq!(choice.confidence, 0.82);
    }

    #[test]
    fn a_question_the_provider_left_unanswered_is_named_and_not_retried() {
        let mut reply = answered_reply();
        reply["answers"]
            .as_object_mut()
            .unwrap()
            .remove("department");

        let message = refused(&reply, &[urgency(), department(), frustration()]);

        assert!(message.contains("department"), "{message}");
    }

    #[test]
    fn an_answer_the_caller_never_asked_for_is_named_and_not_retried() {
        let message = refused(&answered_reply(), &[urgency()]);

        assert!(
            message.contains("department") || message.contains("frustration"),
            "{message}"
        );
    }

    #[test]
    fn an_answer_of_the_wrong_type_names_the_question() {
        let reply = json!({
            "model": "jev-1.13.0",
            "answers": {"is_urgent": {"type": "score", "score": 1.0}},
        });

        let message = refused(&reply, &[urgency()]);

        assert!(message.contains("is_urgent"), "{message}");
        assert!(message.contains("noul"), "{message}");
    }

    #[test]
    fn a_noul_the_provider_did_not_give_is_never_read_as_a_confident_no() {
        for answered in [
            json!({"type": "noul"}),
            json!({"type": "noul", "noul": null}),
            json!({"type": "noul", "noul": "0.92"}),
        ] {
            let reply = json!({"model": "jev-1.13.0", "answers": {"is_urgent": answered}});

            let message = refused(&reply, &[urgency()]);

            assert!(message.contains("is_urgent"), "{message}");
            assert!(message.contains("noul"), "{message}");
        }
    }

    #[test]
    fn a_noul_outside_zero_to_one_names_the_question() {
        let reply = json!({
            "model": "jev-1.13.0",
            "answers": {"is_urgent": {"type": "noul", "noul": 1.4}},
        });

        assert!(refused(&reply, &[urgency()]).contains("is_urgent"));
    }

    #[test]
    fn a_score_the_provider_did_not_give_is_never_read_as_the_lowest_level() {
        let message = refused(
            &scored_reply(json!({"type": "score", "confidence": 0.5})),
            &[frustration()],
        );

        assert!(message.contains("frustration"), "{message}");
        assert!(message.contains("score"), "{message}");
    }

    #[test]
    fn a_score_past_the_scale_the_caller_sent_names_the_question_and_the_scale() {
        for off_the_scale in [json!(47.0), json!(3.0), json!(-0.5)] {
            let message = refused(
                &scored_reply(json!({"type": "score", "score": off_the_scale})),
                &[frustration()],
            );

            assert!(message.contains("frustration"), "{message}");
            assert!(message.contains("0 to 2"), "the scale is named: {message}");
        }
    }

    #[test]
    fn a_score_at_either_end_of_the_scale_is_accepted() {
        for on_the_scale in [0.0, 2.0] {
            let reply = scored_reply(json!({"type": "score", "score": on_the_scale}));

            read_verdict(&reply, &[frustration()], &json!({})).unwrap();
        }
    }

    #[test]
    fn a_choice_the_caller_never_offered_names_the_question() {
        let reply = json!({
            "model": "jev-1.13.0",
            "answers": {"department": {"type": "choice", "choice": "legal"}},
        });

        let message = refused(&reply, &[department()]);

        assert!(message.contains("department"), "{message}");
        assert!(message.contains("legal"), "{message}");
    }

    #[test]
    fn a_probability_for_an_option_that_was_never_offered_names_it() {
        let reply = json!({
            "model": "jev-1.13.0",
            "answers": {"department": {
                "type": "choice",
                "choice": "billing",
                "probabilities": {"billing": 0.7, "legal": 0.3},
            }},
        });

        let message = refused(&reply, &[department()]);

        assert!(message.contains("department"), "{message}");
        assert!(message.contains("legal"), "{message}");
    }

    #[test]
    fn probabilities_that_are_not_an_object_name_the_question_on_either_path() {
        for (answered, question) in [
            (
                json!({"model": "jev-1.13.0", "answers": {"department": {
                    "type": "choice", "choice": "billing", "probabilities": 0.5,
                }}}),
                department(),
            ),
            (
                scored_reply(json!({"type": "score", "score": 1.0, "probabilities": 0.5})),
                frustration(),
            ),
        ] {
            let message = refused(&answered, &[question]);

            assert!(message.contains("probabilities"), "{message}");
        }
    }

    #[test]
    fn a_level_past_the_scale_the_caller_sent_names_the_question() {
        let reply =
            scored_reply(json!({"type": "score", "score": 1.0, "probabilities": {"3": 0.5}}));

        let message = refused(&reply, &[frustration()]);

        assert!(message.contains("frustration"), "{message}");
        assert!(message.contains("level 3"), "{message}");
    }

    #[test]
    fn a_score_level_that_is_not_a_number_names_the_question() {
        let reply =
            scored_reply(json!({"type": "score", "score": 1.0, "probabilities": {"calm": 0.5}}));

        assert!(refused(&reply, &[frustration()]).contains("frustration"));
    }

    #[test]
    fn the_legend_is_the_scale_the_caller_sent_rather_than_the_providers_echo_of_it() {
        let reply = scored_reply(json!({
            "type": "score",
            "score": 1.0,
            "legend": {"0": "Serene", "1": "Cross", "2": "Livid"},
        }));

        let verdict = read_verdict(&reply, &[frustration()], &json!({})).unwrap();

        let Some(Given::Score(score)) = &verdict.answers[0].answer else {
            panic!("a score question is answered with a score");
        };
        assert_eq!(score.legend[&0], "Calm");
        assert_eq!(score.legend[&1], "Frustrated");
        assert_eq!(score.legend[&2], "Very angry");
    }

    #[test]
    fn an_optional_the_provider_left_out_or_sent_as_null_reads_the_same_way() {
        for answered in [
            json!({"type": "score", "score": 1.0}),
            json!({"type": "score", "score": 1.0, "probabilities": null, "confidence": null}),
        ] {
            let verdict =
                read_verdict(&scored_reply(answered), &[frustration()], &json!({})).unwrap();

            let Some(Given::Score(score)) = &verdict.answers[0].answer else {
                panic!("a score question is answered with a score");
            };
            assert!(score.probabilities.is_empty());
            assert_eq!(score.confidence, 0.0);
            assert_eq!(score.legend.len(), 3, "the legend is always the caller's");
        }
    }

    #[test]
    fn a_probability_outside_zero_to_one_names_the_question() {
        let reply =
            scored_reply(json!({"type": "score", "score": 1.0, "probabilities": {"1": 1.5}}));

        assert!(refused(&reply, &[frustration()]).contains("frustration"));
    }

    #[test]
    fn a_confidence_the_provider_sent_but_we_cannot_read_names_the_question() {
        let reply = scored_reply(json!({"type": "score", "score": 1.0, "confidence": "high"}));

        let message = refused(&reply, &[frustration()]);

        assert!(message.contains("frustration"), "{message}");
        assert!(message.contains("confidence"), "{message}");
    }

    #[test]
    fn a_reply_without_answers_is_worth_retrying() {
        assert!(matches!(
            read_verdict(&json!({"model": "jev-1.13.0"}), &[urgency()], &json!({})).unwrap_err(),
            CallError::WorthRetrying(_)
        ));
    }

    #[test]
    fn a_model_the_provider_did_not_name_is_read_as_a_card_of_empty_fields() {
        let card = model_card(&json!({"name": "jev-latest"}));

        assert_eq!(card.name, "jev-latest");
        assert!(card.description.is_empty());
        assert!(card.release_date.is_empty());
    }
}
