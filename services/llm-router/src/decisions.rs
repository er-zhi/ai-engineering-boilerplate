// The System One class: one vendor, one model, and no fallback to fall back to. Records what the call cost
// and hands a provider's refusal of the caller's own body back as the caller's. The budget it checks that
// body against lives in budget.rs.

use std::time::Instant;

use common::proto::llm_router::v1::{DecideRequest, DecideResponse, DescribeModelsResponse};
use connectrpc::ConnectError;
use serde_json::json;

use crate::audit::request::Outcome;
use crate::budget::{published, validated_decision};
use crate::decision::Decider;
use crate::log::{DecisionAttempt, DecisionLog};
use crate::provider::CallError;
use crate::wire::decide_response_from;

const NO_DECISION: &str = "the model did not decide; the decision log holds the provider's reply";
const NOT_CONFIGURED: &str = "structured decisions are not configured for this deployment";
const PROVIDER_REFUSED: &str =
    "the provider refused this decision; the decision log holds the provider's reply";
const UNLISTED: &str = "the provider did not list its models";
// DescribeModels makes no decision and writes no row, so there is no log entry to send an operator to.
const CANNOT_LIST: &str =
    "the models of this class could not be listed; this service's own logs hold why";

pub struct Decisions<D: Decider, L: DecisionLog> {
    // The decider and the model it is asked for are one setting: a deployment either serves this class or
    // does not, and holding them apart let "no decider" and "no model" disagree.
    configured: Option<(D, String)>,
    log: L,
}

impl<D: Decider, L: DecisionLog> Decisions<D, L> {
    pub fn new(configured: Option<(D, String)>, log: L) -> Self {
        Self { configured, log }
    }

    pub async fn decide(&self, request: DecideRequest) -> Result<DecideResponse, ConnectError> {
        let (decider, model) = self.configured()?;
        let decision = validated_decision(request)?;
        let questions = i16::try_from(decision.questions.len()).unwrap_or(i16::MAX);

        let started = Instant::now();
        let decided = decider.decide(model, &decision).await;
        let latency_ms = i32::try_from(started.elapsed().as_millis()).unwrap_or(i32::MAX);

        match decided {
            Ok(verdict) => {
                self.record(DecisionAttempt {
                    model_used: verdict.model_used.clone(),
                    questions,
                    tokens_in: verdict.tokens_in,
                    tokens_out: verdict.tokens_out,
                    latency_ms,
                    outcome: Outcome::Answered,
                    sent: verdict.sent.clone(),
                    received: verdict.reply.clone(),
                })
                .await;

                Ok(decide_response_from(verdict))
            }
            Err(failed) => {
                let message = failed.error.message().to_owned();
                self.record(DecisionAttempt {
                    model_used: model.to_owned(),
                    questions,
                    tokens_in: 0,
                    tokens_out: 0,
                    latency_ms,
                    outcome: Outcome::Failed,
                    sent: failed.sent,
                    received: json!({"error": message}),
                })
                .await;

                tracing::warn!(model, "no decision: {message}");
                Err(reported(&failed.error))
            }
        }
    }

    pub async fn describe_models(&self) -> Result<DescribeModelsResponse, ConnectError> {
        let (decider, _) = self.configured()?;
        let models = decider.models().await.map_err(|failed| {
            tracing::warn!("could not list the System One models: {}", failed.message());
            // A listing carries nothing of the caller's, so a refusal here is this deployment's fault.
            match failed {
                CallError::WorthRetrying(_) => ConnectError::unavailable(UNLISTED),
                CallError::Refused(_) | CallError::Final(_) => ConnectError::internal(CANNOT_LIST),
            }
        })?;

        Ok(DescribeModelsResponse {
            models,
            budget: published().into(),
            ..Default::default()
        })
    }

    fn configured(&self) -> Result<(&D, &str), ConnectError> {
        self.configured
            .as_ref()
            .map(|(decider, model)| (decider, model.as_str()))
            .ok_or_else(|| ConnectError::failed_precondition(NOT_CONFIGURED))
    }

    async fn record(&self, attempt: DecisionAttempt) {
        if let Err(error) = self.log.record_decision(attempt).await {
            tracing::error!("could not record a decision: {error}");
        }
    }
}

// The caller authors the state, the instructions, the options and the levels, so a provider that refuses the
// body is handing their own request back, with its own words. A key, a balance or a base URL is not their
// request, and the adapter has already told those apart.
fn reported(failed: &CallError) -> ConnectError {
    match failed {
        CallError::WorthRetrying(_) => ConnectError::unavailable(NO_DECISION),
        CallError::Refused(message) => ConnectError::invalid_argument(message.clone()),
        CallError::Final(_) => ConnectError::internal(PROVIDER_REFUSED),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use common::proto::llm_router::v1::{ModelCard, NoulAnswer};
    use connectrpc::ErrorCode;
    use sea_orm::DbErr;
    use serde_json::Value;

    use super::*;
    use crate::budget::MAX_QUESTIONS;
    use crate::decision::{Decision, NotDecided, Verdict};
    use crate::test_system_one::{deciding, noul_question};

    type Listed = Result<Vec<ModelCard>, CallError>;

    #[derive(Clone)]
    struct ScriptedDecider {
        replies: Arc<Mutex<Vec<Result<Verdict, NotDecided>>>>,
        listing: Arc<Mutex<Vec<Listed>>>,
        asked: Arc<Mutex<Vec<(String, Decision)>>>,
    }

    impl ScriptedDecider {
        fn new(replies: Vec<Result<Verdict, NotDecided>>) -> Self {
            Self::with(replies, Ok(vec![jev_card()]))
        }

        fn listing(listed: Listed) -> Self {
            Self::with(Vec::new(), listed)
        }

        fn with(replies: Vec<Result<Verdict, NotDecided>>, listed: Listed) -> Self {
            Self {
                replies: Arc::new(Mutex::new(replies)),
                listing: Arc::new(Mutex::new(vec![listed])),
                asked: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn asked(&self) -> Vec<(String, Decision)> {
            self.asked.lock().unwrap().clone()
        }
    }

    impl Decider for ScriptedDecider {
        async fn decide(&self, model: &str, decision: &Decision) -> Result<Verdict, NotDecided> {
            self.asked
                .lock()
                .unwrap()
                .push((model.to_owned(), decision.clone()));
            self.replies.lock().unwrap().remove(0)
        }

        async fn models(&self) -> Result<Vec<ModelCard>, CallError> {
            self.listing.lock().unwrap().remove(0)
        }
    }

    #[derive(Clone)]
    struct RecordedDecisions {
        attempts: Arc<Mutex<Vec<DecisionAttempt>>>,
    }

    impl RecordedDecisions {
        fn new() -> Self {
            Self {
                attempts: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn attempts(&self) -> usize {
            self.attempts.lock().unwrap().len()
        }
    }

    impl DecisionLog for RecordedDecisions {
        async fn record_decision(&self, attempt: DecisionAttempt) -> Result<(), DbErr> {
            self.attempts.lock().unwrap().push(attempt);
            Ok(())
        }
    }

    fn jev_card() -> ModelCard {
        ModelCard {
            name: "jev-latest".to_owned(),
            description: "The current judgement model".to_owned(),
            ..Default::default()
        }
    }

    fn sent_body() -> Value {
        json!({
            "model": "jev-latest",
            "state": "Help! My payouts have been failing for 3 days.",
            "questions": [{"id": "is_urgent", "type": "noul", "instructions": "Does this convey urgency?"}],
        })
    }

    fn decided() -> Result<Verdict, NotDecided> {
        Ok(Verdict {
            model_used: "jev-1.13.0".to_owned(),
            answers: vec![common::proto::llm_router::v1::Answer {
                id: "is_urgent".to_owned(),
                answer: NoulAnswer {
                    noul: 0.92,
                    ..Default::default()
                }
                .into(),
                ..Default::default()
            }],
            tokens_in: 312,
            tokens_out: 48,
            sent: sent_body(),
            reply: json!({"answers": {"is_urgent": {"type": "noul", "noul": 0.92}}}),
        })
    }

    fn not_decided(error: CallError) -> Result<Verdict, NotDecided> {
        Err(NotDecided {
            sent: sent_body(),
            error,
        })
    }

    fn decisions(
        decider: Option<ScriptedDecider>,
        log: RecordedDecisions,
    ) -> Decisions<ScriptedDecider, RecordedDecisions> {
        Decisions::new(
            decider.map(|decider| (decider, "jev-latest".to_owned())),
            log,
        )
    }

    #[tokio::test(start_paused = true)]
    async fn a_decided_call_comes_back_with_its_model_and_is_recorded_with_what_was_sent() {
        let decider = ScriptedDecider::new(vec![decided()]);
        let log = RecordedDecisions::new();
        let service = decisions(Some(decider.clone()), log.clone());

        let response = service
            .decide(deciding(vec![noul_question("is_urgent")]))
            .await
            .unwrap();

        assert_eq!(response.model_used, "jev-1.13.0");
        assert_eq!(response.answers.len(), 1);
        assert_eq!(response.tokens_in, 312);
        assert_eq!(response.tokens_out, 48);
        assert_eq!(decider.asked()[0].0, "jev-latest");

        let recorded = log.attempts.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].outcome, Outcome::Answered);
        assert_eq!(recorded[0].model_used, "jev-1.13.0");
        assert_eq!(recorded[0].questions, 1);
        assert_eq!(recorded[0].tokens_in, 312);
        assert_eq!(
            recorded[0].sent,
            sent_body(),
            "the audit copy is the body the adapter actually sent"
        );
        assert_eq!(recorded[0].received["answers"]["is_urgent"]["noul"], 0.92);
    }

    #[tokio::test(start_paused = true)]
    async fn a_decision_the_provider_could_not_make_is_reported_and_recorded_as_failed() {
        let decider = ScriptedDecider::new(vec![not_decided(CallError::WorthRetrying(
            "the provider answered 529".to_owned(),
        ))]);
        let log = RecordedDecisions::new();
        let service = decisions(Some(decider), log.clone());

        let error = service
            .decide(deciding(vec![noul_question("is_urgent")]))
            .await
            .unwrap_err();

        assert_eq!(error.code, ErrorCode::Unavailable, "{error:?}");
        let reported = format!("{error:?}");
        assert!(reported.contains("did not decide"), "{reported}");
        assert!(
            !reported.contains("529"),
            "the provider's own words must stay in the log: {reported}"
        );
        let recorded = log.attempts.lock().unwrap();
        assert_eq!(recorded[0].outcome, Outcome::Failed);
        assert_eq!(recorded[0].model_used, "jev-latest");
        assert_eq!(recorded[0].tokens_out, 0);
        assert_eq!(recorded[0].sent, sent_body());
        assert!(
            recorded[0].received["error"]
                .as_str()
                .unwrap()
                .contains("529")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_request_the_provider_refused_comes_back_to_the_caller_in_the_providers_words() {
        let decider = ScriptedDecider::new(vec![not_decided(CallError::Refused(
            "the provider answered 422: body.questions: Dictionary should have at least 1 item"
                .to_owned(),
        ))]);
        let log = RecordedDecisions::new();
        let service = decisions(Some(decider), log.clone());

        let error = service
            .decide(deciding(vec![noul_question("is_urgent")]))
            .await
            .unwrap_err();

        assert_eq!(error.code, ErrorCode::InvalidArgument, "{error:?}");
        assert!(
            format!("{error:?}").contains("body.questions"),
            "the caller authored this request and needs to see what was wrong with it: {error:?}"
        );
        assert_eq!(log.attempts.lock().unwrap()[0].outcome, Outcome::Failed);
    }

    #[tokio::test(start_paused = true)]
    async fn a_reply_the_adapter_could_not_make_sense_of_stays_ours_to_own() {
        let decider = ScriptedDecider::new(vec![not_decided(CallError::Final(
            "the provider left is_urgent unanswered".to_owned(),
        ))]);
        let log = RecordedDecisions::new();
        let service = decisions(Some(decider), log.clone());

        let error = service
            .decide(deciding(vec![noul_question("is_urgent")]))
            .await
            .unwrap_err();

        assert_eq!(error.code, ErrorCode::Internal, "{error:?}");
        let reported = format!("{error:?}");
        assert!(reported.contains("refused this decision"), "{reported}");
        assert!(
            !reported.contains("unanswered"),
            "a broken adapter contract is not the caller's to read: {reported}"
        );
        let recorded = log.attempts.lock().unwrap();
        assert_eq!(recorded[0].outcome, Outcome::Failed);
        assert!(
            recorded[0].received["error"]
                .as_str()
                .unwrap()
                .contains("unanswered")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_request_over_the_budget_never_reaches_the_provider_and_is_never_recorded() {
        let decider = ScriptedDecider::new(vec![decided()]);
        let log = RecordedDecisions::new();
        let service = decisions(Some(decider.clone()), log.clone());
        let too_many = (0..=MAX_QUESTIONS)
            .map(|at| noul_question(&format!("question_{at}")))
            .collect();

        let error = service.decide(deciding(too_many)).await.unwrap_err();

        assert_eq!(error.code, ErrorCode::InvalidArgument, "{error:?}");
        assert!(decider.asked().is_empty());
        assert_eq!(log.attempts(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn without_a_key_both_calls_say_so_and_nothing_is_recorded() {
        let log = RecordedDecisions::new();
        let service = decisions(None, log.clone());

        let refused = service
            .decide(deciding(vec![noul_question("is_urgent")]))
            .await
            .unwrap_err();
        let unlisted = service.describe_models().await.unwrap_err();

        for error in [&refused, &unlisted] {
            assert_eq!(error.code, ErrorCode::FailedPrecondition, "{error:?}");
            assert!(
                format!("{error:?}").contains("not configured for this deployment"),
                "{error:?}"
            );
            assert!(
                !format!("{error:?}").contains("TYPESAFE_AI_API_KEY"),
                "a caller cannot act on this deployment's variable names: {error:?}"
            );
        }
        assert_eq!(log.attempts(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn the_models_and_the_budget_of_this_class_are_published_together() {
        let service = decisions(
            Some(ScriptedDecider::listing(Ok(vec![jev_card()]))),
            RecordedDecisions::new(),
        );

        let response = service.describe_models().await.unwrap();

        assert_eq!(response.models.len(), 1);
        assert_eq!(response.models[0].name, "jev-latest");
        assert_eq!(response.budget.into_option(), Some(published()));
    }

    #[tokio::test(start_paused = true)]
    async fn a_listing_the_provider_could_not_give_is_an_error_rather_than_no_models() {
        for (listed, expected) in [
            (
                CallError::Final("a shape this adapter cannot read".to_owned()),
                ErrorCode::Internal,
            ),
            (
                CallError::Refused("the provider answered 401".to_owned()),
                ErrorCode::Internal,
            ),
            (
                CallError::WorthRetrying("the provider answered 503".to_owned()),
                ErrorCode::Unavailable,
            ),
        ] {
            let service = decisions(
                Some(ScriptedDecider::listing(Err(listed))),
                RecordedDecisions::new(),
            );

            let error = service.describe_models().await.unwrap_err();

            assert_eq!(error.code, expected, "{error:?}");
            assert!(
                !format!("{error:?}").contains("decision log"),
                "a listing writes no decision row to send an operator to: {error:?}"
            );
        }
    }
}
