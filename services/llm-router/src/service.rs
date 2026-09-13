// Checks each request against the tier contract, routes it to a model, and records what the call cost and how it went.

use std::time::Instant;

use common::llm::{Sampling, fit_to_limits, limits};
use common::proto::llm_router::v1::{CompleteRequest, CompleteResponse, FinishReason};
use connectrpc::ConnectError;
use serde_json::json;

use crate::adapters::openai_compatible::request_body;
use crate::entity::request::Outcome;
use crate::log::{Attempt, RequestLog};
use crate::provider::{CallError, Prompt, Provider};
use crate::router;
use crate::tiers::Tiers;
use crate::wire::{known_tier, response_from, sampling_from};

const NO_MODEL_ANSWERED: &str =
    "no model on this tier answered; the request log holds the provider's reply";
const PROVIDER_REFUSED: &str =
    "the provider refused this request; the request log holds the provider's reply";
const MAX_STOP_SEQUENCES: usize = 4;
const MAX_STOP_SEQUENCE_BYTES: usize = 1024;
const MAX_TOTAL_STOP_BYTES: usize = 2048;
const MAX_LOGIT_BIAS_ENTRIES: usize = 300;
const MAX_TOP_LOGPROBS: i32 = 20;

pub struct Router<P: Provider, L: RequestLog> {
    provider: P,
    log: L,
    tiers: Tiers,
}

impl<P: Provider, L: RequestLog> Router<P, L> {
    pub fn new(provider: P, log: L, tiers: Tiers) -> Self {
        Self {
            provider,
            log,
            tiers,
        }
    }

    pub async fn complete(
        &self,
        request: CompleteRequest,
    ) -> Result<CompleteResponse, ConnectError> {
        let tier = known_tier(request.tier);
        let Some(contract) = limits(tier) else {
            return Err(ConnectError::invalid_argument(format!(
                "tier {tier:?} is not served"
            )));
        };
        if request.user_prompt.trim().is_empty() {
            return Err(ConnectError::invalid_argument("user_prompt is required"));
        }

        let mut sampling = sampling_from(request.sampling.into_option().unwrap_or_default());
        validate_sampling(&sampling)?;
        fit_to_limits(
            &request.system_prompt,
            &request.user_prompt,
            &mut sampling,
            contract,
        )
        .map_err(ConnectError::invalid_argument)?;

        let prompt = Prompt {
            tier,
            system: request.system_prompt,
            user: request.user_prompt,
            sampling,
        };

        let started = Instant::now();
        let routed = router::complete(&self.provider, &self.tiers, &prompt).await;
        let latency_ms = i32::try_from(started.elapsed().as_millis()).unwrap_or(i32::MAX);

        match routed {
            Ok(answer) => {
                self.record(Attempt {
                    tier,
                    model_used: answer.model_used.clone(),
                    used_backup: answer.used_backup,
                    tokens_in: answer.completion.tokens_in,
                    tokens_out: answer.completion.tokens_out,
                    latency_ms,
                    outcome: Outcome::Answered,
                    finish_reason: answer.completion.finish_reason,
                    sent: request_body(&answer.model_used, &prompt),
                    received: answer.completion.reply.clone(),
                })
                .await;

                Ok(response_from(answer))
            }
            Err(failed) => {
                let (CallError::WorthRetrying(message) | CallError::Final(message)) = &failed.error;
                self.record(Attempt {
                    tier,
                    model_used: failed.model_used.clone(),
                    used_backup: failed.used_backup,
                    tokens_in: 0,
                    tokens_out: 0,
                    latency_ms,
                    outcome: Outcome::Failed,
                    finish_reason: FinishReason::Unspecified,
                    sent: request_body(&failed.model_used, &prompt),
                    received: json!({"error": message}),
                })
                .await;

                tracing::warn!(
                    tier = ?tier,
                    model = failed.model_used,
                    "no answer: {message}"
                );
                Err(match failed.error {
                    CallError::WorthRetrying(_) => ConnectError::unavailable(NO_MODEL_ANSWERED),
                    CallError::Final(_) => ConnectError::internal(PROVIDER_REFUSED),
                })
            }
        }
    }

    async fn record(&self, attempt: Attempt) {
        if let Err(error) = self.log.record(attempt).await {
            tracing::error!("could not record an llm call: {error}");
        }
    }
}

fn validate_sampling(sampling: &Sampling) -> Result<(), ConnectError> {
    validate_number("temperature", sampling.temperature, 0.0, 2.0)?;
    validate_number("top_p", sampling.top_p, 0.0, 1.0)?;
    validate_number("frequency_penalty", sampling.frequency_penalty, -2.0, 2.0)?;
    validate_number("presence_penalty", sampling.presence_penalty, -2.0, 2.0)?;
    validate_stop_sequences(&sampling.stop)?;
    validate_logit_bias(sampling)?;
    if sampling
        .top_logprobs
        .is_some_and(|value| !(0..=MAX_TOP_LOGPROBS).contains(&value))
    {
        return Err(ConnectError::invalid_argument(format!(
            "top_logprobs must be between 0 and {MAX_TOP_LOGPROBS}"
        )));
    }
    Ok(())
}

fn validate_number(
    field: &str,
    value: Option<f64>,
    minimum: f64,
    maximum: f64,
) -> Result<(), ConnectError> {
    if value.is_some_and(|value| !value.is_finite() || !(minimum..=maximum).contains(&value)) {
        return Err(ConnectError::invalid_argument(format!(
            "{field} must be a finite number between {minimum} and {maximum}"
        )));
    }
    Ok(())
}

fn validate_stop_sequences(stop: &[String]) -> Result<(), ConnectError> {
    if stop.len() > MAX_STOP_SEQUENCES {
        return Err(ConnectError::invalid_argument(format!(
            "stop must hold at most {MAX_STOP_SEQUENCES} sequences"
        )));
    }
    if stop
        .iter()
        .any(|sequence| sequence.len() > MAX_STOP_SEQUENCE_BYTES)
    {
        return Err(ConnectError::invalid_argument(format!(
            "each stop sequence must be at most {MAX_STOP_SEQUENCE_BYTES} bytes"
        )));
    }
    let total_bytes = stop.iter().fold(0_usize, |total, sequence| {
        total.saturating_add(sequence.len())
    });
    if total_bytes > MAX_TOTAL_STOP_BYTES {
        return Err(ConnectError::invalid_argument(format!(
            "stop sequences must total at most {MAX_TOTAL_STOP_BYTES} bytes"
        )));
    }
    Ok(())
}

fn validate_logit_bias(sampling: &Sampling) -> Result<(), ConnectError> {
    if sampling.logit_bias.len() > MAX_LOGIT_BIAS_ENTRIES {
        return Err(ConnectError::invalid_argument(format!(
            "logit_bias must hold at most {MAX_LOGIT_BIAS_ENTRIES} entries"
        )));
    }
    for value in sampling.logit_bias.values() {
        validate_number("each logit_bias value", Some(*value), -100.0, 100.0)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use buffa::EnumValue;
    use chrono::{DateTime, Utc};
    use common::llm::limits as tier_limits;
    use common::proto::llm_router::v1::{QualityTier, Sampling as WireSampling};
    use sea_orm::DbErr;

    use super::*;
    use crate::provider::Completion;

    #[derive(Clone)]
    struct Scripted {
        replies: Arc<Mutex<Vec<Result<Completion, CallError>>>>,
        asked: Arc<Mutex<Vec<(String, Prompt)>>>,
    }

    impl Scripted {
        fn new(replies: Vec<Result<Completion, CallError>>) -> Self {
            Self {
                replies: Arc::new(Mutex::new(replies)),
                asked: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn asked(&self) -> Vec<(String, Prompt)> {
            self.asked.lock().unwrap().clone()
        }
    }

    impl Provider for Scripted {
        async fn complete(&self, model: &str, prompt: &Prompt) -> Result<Completion, CallError> {
            self.asked
                .lock()
                .unwrap()
                .push((model.to_owned(), prompt.clone()));
            self.replies.lock().unwrap().remove(0)
        }
    }

    #[derive(Clone)]
    struct Recorded {
        attempts: Arc<Mutex<Vec<Attempt>>>,
    }

    impl Recorded {
        fn new() -> Self {
            Self {
                attempts: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn attempts(&self) -> usize {
            self.attempts.lock().unwrap().len()
        }
    }

    impl RequestLog for Recorded {
        async fn record(&self, attempt: Attempt) -> Result<(), DbErr> {
            self.attempts.lock().unwrap().push(attempt);
            Ok(())
        }

        async fn drop_expired_payloads(&self, _now: DateTime<Utc>) -> Result<u64, DbErr> {
            Ok(0)
        }
    }

    fn answered() -> Result<Completion, CallError> {
        Ok(Completion {
            content: "docs".to_owned(),
            tokens_in: 10,
            tokens_out: 2,
            finish_reason: FinishReason::Stop,
            reply: json!({"choices": [{"message": {"content": "docs"}}]}),
        })
    }

    fn tiers() -> Tiers {
        Tiers::from_vars(|name| match name {
            "LLM_LOW_PRIMARY" => Some("deepseek/deepseek-v4-flash".to_owned()),
            "LLM_LOW_BACKUP" => Some("z-ai/glm-4.7-flash".to_owned()),
            _ => Some(format!("model-for-{name}")),
        })
        .unwrap()
    }

    fn asking(tier: QualityTier, user_prompt: &str) -> CompleteRequest {
        CompleteRequest {
            tier: EnumValue::Known(tier),
            system_prompt: "You label pages.".to_owned(),
            user_prompt: user_prompt.to_owned(),
            ..Default::default()
        }
    }

    async fn assert_sampling_refused(sampling: WireSampling, field: &str) {
        let provider = Scripted::new(vec![answered()]);
        let log = Recorded::new();
        let router = Router::new(provider.clone(), log.clone(), tiers());

        let error = router
            .complete(CompleteRequest {
                sampling: sampling.into(),
                ..asking(QualityTier::Low, "https://example.com")
            })
            .await
            .unwrap_err();

        assert!(format!("{error:?}").contains(field), "{error:?}");
        assert!(provider.asked().is_empty());
        assert_eq!(log.attempts(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn an_answered_call_comes_back_with_its_model_and_is_recorded_with_both_payloads() {
        let provider = Scripted::new(vec![answered()]);
        let log = Recorded::new();
        let router = Router::new(provider.clone(), log.clone(), tiers());

        let response = router
            .complete(asking(QualityTier::Low, "https://example.com"))
            .await
            .unwrap();

        assert_eq!(response.content, "docs");
        assert_eq!(response.model_used, "deepseek/deepseek-v4-flash");
        assert!(!response.used_backup);

        let recorded = log.attempts.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].outcome, Outcome::Answered);
        assert_eq!(recorded[0].model_used, "deepseek/deepseek-v4-flash");
        assert_eq!(recorded[0].tokens_in, 10);
        assert_eq!(recorded[0].tokens_out, 2);
        assert_eq!(
            recorded[0].sent["model"],
            json!("deepseek/deepseek-v4-flash")
        );
        assert_eq!(
            recorded[0].received["choices"][0]["message"]["content"],
            json!("docs")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_call_no_model_could_answer_is_reported_and_recorded_as_failed() {
        let provider = Scripted::new(vec![
            Err(CallError::WorthRetrying("primary timed out".to_owned())),
            Err(CallError::WorthRetrying("backup timed out".to_owned())),
        ]);
        let log = Recorded::new();
        let router = Router::new(provider, log.clone(), tiers());

        let error = router
            .complete(asking(QualityTier::Low, "https://example.com"))
            .await
            .unwrap_err();

        let reported = format!("{error:?}");
        assert!(reported.contains("no model"), "{reported}");
        assert!(
            !reported.contains("backup timed out"),
            "the provider's own words must stay in the log: {reported}"
        );
        let recorded = log.attempts.lock().unwrap();
        assert_eq!(recorded[0].outcome, Outcome::Failed);
        assert_eq!(recorded[0].model_used, "z-ai/glm-4.7-flash");
        assert_eq!(recorded[0].tokens_out, 0);
        assert!(
            recorded[0].received["error"]
                .as_str()
                .unwrap()
                .contains("backup timed out")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_tier_the_router_does_not_serve_is_refused_before_any_model_is_called() {
        let provider = Scripted::new(vec![answered()]);
        let log = Recorded::new();
        let router = Router::new(provider.clone(), log.clone(), tiers());

        let error = router
            .complete(asking(QualityTier::Unspecified, "https://example.com"))
            .await
            .unwrap_err();

        assert!(format!("{error:?}").contains("tier"), "{error:?}");
        assert!(provider.asked().is_empty());
        assert_eq!(log.attempts(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn an_empty_prompt_is_refused_before_any_model_is_called() {
        let provider = Scripted::new(vec![answered()]);
        let log = Recorded::new();
        let router = Router::new(provider.clone(), log.clone(), tiers());

        let error = router
            .complete(asking(QualityTier::Low, ""))
            .await
            .unwrap_err();

        assert!(format!("{error:?}").contains("user_prompt"), "{error:?}");
        assert!(provider.asked().is_empty());
        assert_eq!(log.attempts(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn asking_for_more_output_than_the_tier_allows_is_refused_with_the_limit_named() {
        let provider = Scripted::new(vec![answered()]);
        let log = Recorded::new();
        let router = Router::new(provider.clone(), log.clone(), tiers());
        let over_the_limit = tier_limits(QualityTier::Low).unwrap().max_output_tokens + 1;

        let error = router
            .complete(CompleteRequest {
                sampling: WireSampling {
                    max_tokens: Some(over_the_limit),
                    ..Default::default()
                }
                .into(),
                ..asking(QualityTier::Low, "https://example.com")
            })
            .await
            .unwrap_err();

        assert!(
            format!("{error:?}").contains(
                &tier_limits(QualityTier::Low)
                    .unwrap()
                    .max_output_tokens
                    .to_string()
            ),
            "{error:?}"
        );
        assert!(provider.asked().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn temperature_outside_zero_to_two_or_non_finite_is_refused() {
        for temperature in [-0.1, 2.1, f64::NAN, f64::INFINITY] {
            assert_sampling_refused(
                WireSampling {
                    temperature: Some(temperature),
                    ..Default::default()
                },
                "temperature",
            )
            .await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn top_p_outside_zero_to_one_is_refused() {
        for top_p in [-0.1, 1.1] {
            assert_sampling_refused(
                WireSampling {
                    top_p: Some(top_p),
                    ..Default::default()
                },
                "top_p",
            )
            .await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn frequency_penalty_outside_minus_two_to_two_is_refused() {
        assert_sampling_refused(
            WireSampling {
                frequency_penalty: Some(2.1),
                ..Default::default()
            },
            "frequency_penalty",
        )
        .await;
    }

    #[tokio::test(start_paused = true)]
    async fn presence_penalty_outside_minus_two_to_two_is_refused() {
        assert_sampling_refused(
            WireSampling {
                presence_penalty: Some(-2.1),
                ..Default::default()
            },
            "presence_penalty",
        )
        .await;
    }

    #[tokio::test(start_paused = true)]
    async fn more_than_four_stop_sequences_are_refused() {
        assert_sampling_refused(
            WireSampling {
                stop: vec!["x".to_owned(); MAX_STOP_SEQUENCES + 1],
                ..Default::default()
            },
            "stop",
        )
        .await;
    }

    #[tokio::test(start_paused = true)]
    async fn an_individual_stop_sequence_over_the_byte_limit_is_refused() {
        assert_sampling_refused(
            WireSampling {
                stop: vec!["x".repeat(MAX_STOP_SEQUENCE_BYTES + 1)],
                ..Default::default()
            },
            "stop sequence",
        )
        .await;
    }

    #[tokio::test(start_paused = true)]
    async fn stop_sequences_over_the_total_byte_limit_are_refused() {
        assert_sampling_refused(
            WireSampling {
                stop: vec!["x".repeat(MAX_TOTAL_STOP_BYTES / MAX_STOP_SEQUENCES + 1); 4],
                ..Default::default()
            },
            "stop sequences",
        )
        .await;
    }

    #[tokio::test(start_paused = true)]
    async fn too_many_logit_bias_entries_are_refused() {
        let logit_bias = (0..=MAX_LOGIT_BIAS_ENTRIES)
            .map(|token| (token.to_string(), 0.0))
            .collect();
        assert_sampling_refused(
            WireSampling {
                logit_bias,
                ..Default::default()
            },
            "logit_bias",
        )
        .await;
    }

    #[tokio::test(start_paused = true)]
    async fn logit_bias_values_outside_minus_one_hundred_to_one_hundred_are_refused() {
        assert_sampling_refused(
            WireSampling {
                logit_bias: [("42".to_owned(), 100.1)].into_iter().collect(),
                ..Default::default()
            },
            "logit_bias value",
        )
        .await;
    }

    #[tokio::test(start_paused = true)]
    async fn top_logprobs_outside_zero_to_twenty_are_refused() {
        for top_logprobs in [-1, MAX_TOP_LOGPROBS + 1] {
            assert_sampling_refused(
                WireSampling {
                    top_logprobs: Some(top_logprobs),
                    ..Default::default()
                },
                "top_logprobs",
            )
            .await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn sampling_values_at_every_boundary_are_accepted() {
        let provider = Scripted::new(vec![answered()]);
        let router = Router::new(provider.clone(), Recorded::new(), tiers());

        router
            .complete(CompleteRequest {
                sampling: WireSampling {
                    temperature: Some(2.0),
                    top_p: Some(1.0),
                    stop: vec!["x".repeat(MAX_TOTAL_STOP_BYTES / MAX_STOP_SEQUENCES); 4],
                    frequency_penalty: Some(-2.0),
                    presence_penalty: Some(2.0),
                    logit_bias: (0..MAX_LOGIT_BIAS_ENTRIES)
                        .map(|token| (token.to_string(), 100.0))
                        .collect(),
                    top_logprobs: Some(MAX_TOP_LOGPROBS),
                    ..Default::default()
                }
                .into(),
                ..asking(QualityTier::Low, "https://example.com")
            })
            .await
            .unwrap();

        assert_eq!(provider.asked().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_caller_that_names_no_output_limit_gets_the_tier_maximum() {
        let provider = Scripted::new(vec![answered()]);
        let log = Recorded::new();
        let router = Router::new(provider.clone(), log, tiers());

        router
            .complete(asking(QualityTier::Low, "https://example.com"))
            .await
            .unwrap();

        let asked = provider.asked();
        assert_eq!(
            asked[0].1.sampling.max_tokens,
            Some(tier_limits(QualityTier::Low).unwrap().max_output_tokens)
        );
    }
}
