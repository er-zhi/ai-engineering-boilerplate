// The contract every provider integration implements: one adapter per provider, speaking in prompts and completions rather than HTTP.

use common::proto::llm_router::v1::{FinishReason, QualityTier, Sampling};
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

// Three ways a call ends badly: try the backup, the caller's own request was rejected, or this adapter's
// contract was broken. Only Refused carries a fault the caller could fix, and only on a path the caller authors.
#[derive(Debug, PartialEq, Eq)]
pub enum CallError {
    WorthRetrying(String),
    Refused(String),
    Final(String),
}

impl CallError {
    pub fn message(&self) -> &str {
        let (Self::WorthRetrying(message) | Self::Refused(message) | Self::Final(message)) = self;
        message
    }
}

pub trait Provider: Send + Sync {
    fn complete(
        &self,
        model: &str,
        prompt: &Prompt,
    ) -> impl Future<Output = Result<Completion, CallError>> + Send;
}
