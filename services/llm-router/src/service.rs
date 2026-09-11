// Checks each request against the tier contract, routes it to a model, and records what the call cost and how it went.

use std::time::Instant;

use common::llm::{fit_to_limits, limits};
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

                Err(match failed.error {
                    CallError::WorthRetrying(message) => ConnectError::unavailable(message),
                    CallError::Final(message) => ConnectError::internal(message),
                })
            }
        }
    }

    async fn record(&self, attempt: Attempt) {
        if let Err(error) = self.log.record(attempt).await {
            eprintln!("could not record an llm call: {error}");
        }
    }
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

        async fn drop_payloads_before(&self, _cutoff: DateTime<Utc>) -> Result<u64, DbErr> {
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

    #[tokio::test]
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

    #[tokio::test]
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

        assert!(
            format!("{error:?}").contains("backup timed out"),
            "{error:?}"
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

    #[tokio::test]
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

    #[tokio::test]
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

    #[tokio::test]
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

    #[tokio::test]
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
