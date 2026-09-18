// The contract every System One integration implements: a state and typed questions in, typed calibrated answers out, rather than HTTP.

use common::proto::llm_router::v1::{Answer, ChoiceOption, ModelCard};
use serde_json::Value;

use crate::provider::CallError;

#[derive(Clone, Debug)]
pub struct Decision {
    pub state: Value,
    pub questions: Vec<Question>,
}

#[derive(Clone, Debug)]
pub struct Question {
    pub id: String,
    pub instructions: Value,
    pub kind: QuestionKind,
}

// Score.levels is the one field here whose order the vendor reads: it evaluates them lowest to highest,
// and body.rs sends this Vec's order straight through as that ordered array. Choice.options is a Vec for
// a stable shape, not because order reaches the model — the vendor takes choice criteria as a map keyed
// by name, evaluated in isolation like every other question.
#[derive(Clone, Debug)]
pub enum QuestionKind {
    Noul {
        when_true: Option<String>,
        when_false: Option<String>,
    },
    Choice {
        options: Vec<ChoiceOption>,
    },
    Score {
        levels: Vec<String>,
    },
}

// What the provider was actually sent, carried back by the adapter that sent it so the audit row never guesses.
#[derive(Debug, PartialEq)]
pub struct Verdict {
    pub model_used: String,
    pub answers: Vec<Answer>,
    pub tokens_in: i32,
    pub tokens_out: i32,
    pub sent: Value,
    pub reply: Value,
}

#[derive(Debug, PartialEq)]
pub struct NotDecided {
    pub sent: Value,
    pub error: CallError,
}

pub trait Decider: Send + Sync {
    fn decide(
        &self,
        model: &str,
        decision: &Decision,
    ) -> impl Future<Output = Result<Verdict, NotDecided>> + Send;
    fn models(&self) -> impl Future<Output = Result<Vec<ModelCard>, CallError>> + Send;
}
