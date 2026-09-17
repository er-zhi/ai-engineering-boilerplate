// Runs a prompt on its tier's primary model and falls back to the backup vendor when the failure is worth retrying.

use std::time::Duration;

use crate::provider::{CallError, Completion, Prompt, Provider};
use crate::tiers::Tiers;

const PAUSE_BEFORE_BACKUP: Duration = Duration::from_millis(250);

#[derive(Debug, PartialEq)]
pub struct Answer {
    pub completion: Completion,
    pub model_used: String,
    pub used_backup: bool,
}

#[derive(Debug, PartialEq)]
pub struct Failed {
    pub error: CallError,
    pub model_used: String,
    pub used_backup: bool,
}

pub async fn complete(
    provider: &impl Provider,
    tiers: &Tiers,
    prompt: &Prompt,
) -> Result<Answer, Failed> {
    let Some(tier) = tiers.models(prompt.tier) else {
        return Err(Failed {
            error: CallError::Final(format!(
                "no models are configured for tier {:?}",
                prompt.tier
            )),
            model_used: String::new(),
            used_backup: false,
        });
    };

    match provider.complete(&tier.primary, prompt).await {
        Ok(completion) => Ok(Answer {
            completion,
            model_used: tier.primary.clone(),
            used_backup: false,
        }),
        Err(rejected @ (CallError::Refused(_) | CallError::Final(_))) => Err(Failed {
            error: rejected,
            model_used: tier.primary.clone(),
            used_backup: false,
        }),
        Err(CallError::WorthRetrying(_)) => {
            tokio::time::sleep(PAUSE_BEFORE_BACKUP).await;
            match provider.complete(&tier.backup, prompt).await {
                Ok(completion) => Ok(Answer {
                    completion,
                    model_used: tier.backup.clone(),
                    used_backup: true,
                }),
                Err(error) => Err(Failed {
                    error,
                    model_used: tier.backup.clone(),
                    used_backup: true,
                }),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use common::proto::llm_router::v1::{FinishReason, QualityTier, Sampling};

    use super::*;

    struct Scripted {
        replies: Mutex<Vec<Result<Completion, CallError>>>,
        asked: Mutex<Vec<String>>,
    }

    impl Scripted {
        fn new(replies: Vec<Result<Completion, CallError>>) -> Self {
            Self {
                replies: Mutex::new(replies),
                asked: Mutex::new(Vec::new()),
            }
        }

        fn asked(&self) -> Vec<String> {
            self.asked.lock().unwrap().clone()
        }
    }

    impl Provider for Scripted {
        async fn complete(&self, model: &str, _prompt: &Prompt) -> Result<Completion, CallError> {
            self.asked.lock().unwrap().push(model.to_owned());
            self.replies.lock().unwrap().remove(0)
        }
    }

    fn answered(content: &str) -> Result<Completion, CallError> {
        Ok(Completion {
            content: content.to_owned(),
            tokens_in: 10,
            tokens_out: 2,
            finish_reason: FinishReason::Stop,
            reply: serde_json::json!({"choices": [{"message": {"content": content}}]}),
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

    fn prompt(tier: QualityTier) -> Prompt {
        Prompt {
            tier,
            system: String::new(),
            user: "classify this".to_owned(),
            sampling: Sampling::default(),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_working_primary_is_the_only_model_asked() {
        let provider = Scripted::new(vec![answered("docs")]);

        let answer = complete(&provider, &tiers(), &prompt(QualityTier::Low))
            .await
            .unwrap();

        assert_eq!(answer.completion.content, "docs");
        assert_eq!(answer.model_used, "deepseek/deepseek-v4-flash");
        assert!(!answer.used_backup);
        assert_eq!(provider.asked(), ["deepseek/deepseek-v4-flash"]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_retryable_primary_failure_moves_to_the_backup_vendor() {
        let provider = Scripted::new(vec![
            Err(CallError::WorthRetrying(
                "the provider answered 429".to_owned(),
            )),
            answered("docs"),
        ]);

        let answer = complete(&provider, &tiers(), &prompt(QualityTier::Low))
            .await
            .unwrap();

        assert_eq!(answer.model_used, "z-ai/glm-4.7-flash");
        assert!(answer.used_backup);
        assert_eq!(
            provider.asked(),
            ["deepseek/deepseek-v4-flash", "z-ai/glm-4.7-flash"]
        );
    }

    // A second vendor cannot fix a request the first one read and refused, so neither arm reaches the backup.
    #[tokio::test(start_paused = true)]
    async fn a_rejected_request_never_reaches_the_backup() {
        for rejected in [
            CallError::Final("the provider answered 400".to_owned()),
            CallError::Refused("the provider answered 422".to_owned()),
        ] {
            let provider = Scripted::new(vec![Err(rejected)]);

            let failed = complete(&provider, &tiers(), &prompt(QualityTier::Low))
                .await
                .unwrap_err();

            assert!(
                matches!(failed.error, CallError::Final(_) | CallError::Refused(_)),
                "{:?} is reported as it arrived",
                failed.error
            );
            assert_eq!(failed.model_used, "deepseek/deepseek-v4-flash");
            assert!(!failed.used_backup);
            assert_eq!(provider.asked(), ["deepseek/deepseek-v4-flash"]);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn when_both_models_fail_the_backup_failure_is_reported() {
        let provider = Scripted::new(vec![
            Err(CallError::WorthRetrying("primary timed out".to_owned())),
            Err(CallError::WorthRetrying("backup timed out".to_owned())),
        ]);

        let failed = complete(&provider, &tiers(), &prompt(QualityTier::Low))
            .await
            .unwrap_err();

        let CallError::WorthRetrying(message) = &failed.error else {
            panic!("both vendors failing is still a transient fault");
        };
        assert!(message.contains("backup timed out"), "{message}");
        assert_eq!(failed.model_used, "z-ai/glm-4.7-flash");
        assert!(failed.used_backup);
    }

    #[tokio::test(start_paused = true)]
    async fn an_unset_tier_asks_no_model() {
        let provider = Scripted::new(vec![answered("docs")]);

        let failed = complete(&provider, &tiers(), &prompt(QualityTier::Unspecified))
            .await
            .unwrap_err();

        assert!(matches!(failed.error, CallError::Final(_)));
        assert!(failed.model_used.is_empty());
        assert!(provider.asked().is_empty());
    }
}
