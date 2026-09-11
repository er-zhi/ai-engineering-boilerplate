// The contract every provider integration implements: one adapter per provider, speaking in prompts and completions rather than HTTP.

use common::llm::Sampling;
use common::proto::llm_router::v1::{FinishReason, QualityTier};
use serde_json::Value;

#[derive(Clone, Debug)]
pub struct Prompt {
    pub tier: QualityTier,
    pub system: String,
    pub user: String,
    pub sampling: Sampling,
}

#[derive(Debug, PartialEq)]
pub struct Completion {
    pub content: String,
    pub tokens_in: i32,
    pub tokens_out: i32,
    pub finish_reason: FinishReason,
    pub reply: Value,
}

#[derive(Debug, PartialEq, Eq)]
pub enum CallError {
    WorthRetrying(String),
    Final(String),
}

pub trait Provider: Send + Sync {
    fn complete(
        &self,
        model: &str,
        prompt: &Prompt,
    ) -> impl Future<Output = Result<Completion, CallError>> + Send;
}
