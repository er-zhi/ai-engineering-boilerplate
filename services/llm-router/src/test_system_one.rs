// Test-only provider on 127.0.0.1 that answers the System One paths with a scripted reply and keeps the
// request body byte for byte, plus the one decision every test in this service asks about.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Json;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use common::proto::llm_router::v1::{
    Choice, ChoiceOption, DecideRequest, Noul, Question as ProtoQuestion, Score,
};
use serde_json::{Value, json};

use crate::decision::{Decision, Question, QuestionKind};

pub fn urgency() -> Question {
    Question {
        id: "is_urgent".to_owned(),
        instructions: json!("Does this convey urgency?"),
        kind: QuestionKind::Noul {
            when_true: Some("Explicitly time-sensitive".to_owned()),
            when_false: Some("No urgency expressed".to_owned()),
        },
    }
}

pub fn department() -> Question {
    Question {
        id: "department".to_owned(),
        instructions: json!("Which team should handle this?"),
        kind: QuestionKind::Choice {
            options: vec![
                offer("billing", Some("Payments, invoicing, refunds")),
                offer("technical", Some("Bugs, outages, integrations")),
                offer("sales", None),
            ],
        },
    }
}

pub fn frustration() -> Question {
    Question {
        id: "frustration".to_owned(),
        instructions: json!("How frustrated is the customer?"),
        kind: QuestionKind::Score {
            levels: vec![
                "Calm".to_owned(),
                "Frustrated".to_owned(),
                "Very angry".to_owned(),
            ],
        },
    }
}

pub fn offer(name: &str, description: Option<&str>) -> ChoiceOption {
    ChoiceOption {
        name: name.to_owned(),
        description: description.map(str::to_owned),
        ..Default::default()
    }
}

pub fn decision(questions: Vec<Question>) -> Decision {
    Decision {
        caller_stops_waiting_at: None,
        state: json!("Help! My payouts have been failing for 3 days."),
        questions,
    }
}

pub fn answered_reply() -> Value {
    json!({
        "model": "jev-1.13.0",
        "answers": {
            "is_urgent": {"type": "noul", "noul": 0.92},
            "department": {
                "type": "choice",
                "choice": "billing",
                "probabilities": {"billing": 0.81, "technical": 0.15, "sales": 0.04},
                "confidence": 0.82,
            },
            "frustration": {
                "type": "score",
                "score": 1.6,
                "legend": {"0": "Calm", "1": "Frustrated", "2": "Very angry"},
                "probabilities": {"0": 0.1, "1": 0.3, "2": 0.6},
                "confidence": 0.78,
            },
        },
        "usage": {"input_tokens": 312, "output_tokens": 48},
    })
}

pub fn scored_reply(answer: Value) -> Value {
    json!({"model": "jev-1.13.0", "answers": {"frustration": answer}})
}

// The proto request the questions above arrive as. Every question carries instructions and every request a
// state, because a call without either is refused before it reaches a provider.
pub fn deciding(questions: Vec<ProtoQuestion>) -> DecideRequest {
    deciding_about(
        json!("Help! My payouts have been failing for 3 days."),
        questions,
    )
}

pub fn deciding_about(state: Value, questions: Vec<ProtoQuestion>) -> DecideRequest {
    let mut request: DecideRequest = serde_json::from_value(json!({"state": state})).unwrap();
    request.questions = questions;
    request
}

pub fn instructed(id: &str, instructions: Value) -> ProtoQuestion {
    let mut question: ProtoQuestion =
        serde_json::from_value(json!({"instructions": instructions})).unwrap();
    question.id = id.to_owned();
    question.kind = Noul::default().into();
    question
}

pub fn noul_question(id: &str) -> ProtoQuestion {
    instructed(id, json!("Does this convey urgency?"))
}

pub fn choice_question(id: &str, options: Vec<(&str, &str)>) -> ProtoQuestion {
    let mut question = instructed(id, json!("Which team should handle this?"));
    question.kind = Choice {
        options: options
            .into_iter()
            .map(|(name, description)| offer(name, Some(description)))
            .collect(),
        ..Default::default()
    }
    .into();
    question
}

pub fn score_question(id: &str, levels: &[&str]) -> ProtoQuestion {
    let mut question = instructed(id, json!("How frustrated is the customer?"));
    question.kind = Score {
        levels: levels.iter().map(|level| (*level).to_owned()).collect(),
        ..Default::default()
    }
    .into();
    question
}

pub fn unprocessable_reply() -> Value {
    json!({"detail": [{
        "type": "too_short",
        "loc": ["body", "questions"],
        "msg": "Dictionary should have at least 1 item after validation, not 0",
    }]})
}

// The body TypeSafe AI actually returns for a rejected key, verbatim: a fixture that claims to be the
// provider's own reply has to be it, or the adapter is tested against words nobody sends.
pub fn unauthenticated_reply() -> Value {
    json!({"detail": {
        "error_type": "authentication_error",
        "message": "Cannot authenticate with the server. Please check your API key and try again.",
    }})
}

#[derive(Clone, Debug)]
pub struct Asked {
    pub authorization: String,
    pub body: String,
}

pub struct TestSystemOne {
    base_url: String,
    asked: Arc<Mutex<Vec<Asked>>>,
}

impl TestSystemOne {
    pub async fn answering(status: StatusCode, reply: Value) -> Self {
        Self::start(status, reply, Duration::ZERO, StatusCode::OK, json!({})).await
    }

    pub async fn answering_after(status: StatusCode, reply: Value, delay: Duration) -> Self {
        Self::start(status, reply, delay, StatusCode::OK, json!({})).await
    }

    pub async fn listing(models_status: StatusCode, models: Value) -> Self {
        Self::start(
            StatusCode::OK,
            json!({}),
            Duration::ZERO,
            models_status,
            models,
        )
        .await
    }

    async fn start(
        decide_status: StatusCode,
        decide_reply: Value,
        decide_delay: Duration,
        models_status: StatusCode,
        models: Value,
    ) -> Self {
        let asked = Arc::new(Mutex::new(Vec::new()));

        let app = axum::Router::new()
            .route(
                "/v1/systemone",
                post({
                    let seen = asked.clone();
                    move |headers: HeaderMap, body: String| {
                        let seen = seen.clone();
                        let reply = decide_reply.clone();
                        async move {
                            seen.lock().unwrap().push(Asked {
                                authorization: bearer(&headers),
                                body,
                            });
                            tokio::time::sleep(decide_delay).await;
                            (decide_status, Json(reply))
                        }
                    }
                }),
            )
            .route(
                "/v1/models",
                get(move || async move { (models_status, Json(models)) }),
            );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        Self { base_url, asked }
    }

    pub fn base_url(&self) -> String {
        self.base_url.clone()
    }

    pub fn asked(&self) -> Vec<Asked> {
        self.asked.lock().unwrap().clone()
    }
}

fn bearer(headers: &HeaderMap) -> String {
    headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned()
}
