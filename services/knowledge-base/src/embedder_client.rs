// Calls the native embedder over loopback HTTP and validates its response.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::entity::document::MAX_EMBEDDING_MODEL_CHARS;
use crate::entity::document_chunk::EMBEDDING_DIMENSIONS;
use crate::llm_client::{EmbedKind, Embedded};

#[derive(Serialize)]
struct EmbedRequest<'a> {
    text: &'a str,
}

#[derive(Deserialize)]
struct EmbedResponse {
    values: Vec<f32>,
    model_used: String,
    #[serde(default)]
    truncated: bool,
}

#[derive(Deserialize)]
struct EmbedderError {
    error: String,
}

pub struct EmbedderClient {
    http: reqwest::Client,
    base_url: String,
}

impl EmbedderClient {
    pub fn new(base_url: &str, timeout: Duration) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|error| format!("could not build the embedder client: {error}"))?;
        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_owned(),
        })
    }

    pub async fn embed(&self, content: &str, kind: EmbedKind) -> Result<Embedded, String> {
        let path = match kind {
            EmbedKind::StoredPassage => "/embed/document",
            EmbedKind::SearchQuery => "/embed/query",
        };

        let response = self
            .http
            .post(format!("{}{path}", self.base_url))
            .json(&EmbedRequest { text: content })
            .send()
            .await
            .map_err(|error| {
                if error.is_timeout() {
                    "the native embedder did not answer in time".to_owned()
                } else {
                    format!(
                        "could not reach the native embedder at {}: {error}",
                        self.base_url
                    )
                }
            })?;

        let status = response.status();
        if !status.is_success() {
            let detail = response
                .json::<EmbedderError>()
                .await
                .map(|body| body.error)
                .unwrap_or_else(|_| format!("the embedder answered {status}"));
            return Err(detail);
        }

        let body = response
            .json::<EmbedResponse>()
            .await
            .map_err(|error| format!("the embedder sent a reply we cannot read: {error}"))?;
        if body.truncated {
            tracing::warn!(
                "the embedder truncated a {}-character input to its token window",
                content.chars().count()
            );
        }

        validate_response(body)
    }
}

fn validate_response(body: EmbedResponse) -> Result<Embedded, String> {
    if body.values.len() != EMBEDDING_DIMENSIONS as usize {
        return Err(format!(
            "the embedder returned {} values; expected {EMBEDDING_DIMENSIONS}",
            body.values.len()
        ));
    }
    if body.values.iter().any(|value| !value.is_finite()) {
        return Err("the embedder returned a non-finite value".to_owned());
    }
    if body.model_used.chars().count() > MAX_EMBEDDING_MODEL_CHARS {
        return Err(format!(
            "the embedder's model name is longer than {MAX_EMBEDDING_MODEL_CHARS} characters"
        ));
    }

    Ok(Embedded {
        values: body.values,
        model_used: body.model_used,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::extract::State;
    use axum::http::{StatusCode, header};
    use axum::response::{IntoResponse, Response};
    use axum::routing::post;
    use axum::{Json, Router};
    use serde_json::{Value, json};

    use super::*;

    #[derive(Clone)]
    enum FakeReply {
        Success,
        Malformed,
        Rejected,
        Delayed,
    }

    #[derive(Clone)]
    struct FakeEmbedder {
        received: Arc<Mutex<Vec<(&'static str, Value)>>>,
        reply: FakeReply,
    }

    async fn document(State(fake): State<FakeEmbedder>, Json(request): Json<Value>) -> Response {
        reply("document", fake, request).await
    }

    async fn query(State(fake): State<FakeEmbedder>, Json(request): Json<Value>) -> Response {
        reply("query", fake, request).await
    }

    async fn reply(path: &'static str, fake: FakeEmbedder, request: Value) -> Response {
        fake.received.lock().unwrap().push((path, request));
        match fake.reply {
            FakeReply::Success => Json(json!({
                "values": vec![0.25; EMBEDDING_DIMENSIONS as usize],
                "model_used": "test-embedder",
                "truncated": false
            }))
            .into_response(),
            FakeReply::Malformed => (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/json")],
                "{not-json",
            )
                .into_response(),
            FakeReply::Rejected => (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "configured rejection"})),
            )
                .into_response(),
            FakeReply::Delayed => {
                tokio::time::sleep(Duration::from_millis(100)).await;
                Json(json!({
                    "values": vec![0.25; EMBEDDING_DIMENSIONS as usize],
                    "model_used": "test-embedder"
                }))
                .into_response()
            }
        }
    }

    async fn start_fake_embedder(reply: FakeReply) -> (String, FakeEmbedder) {
        let fake = FakeEmbedder {
            received: Arc::new(Mutex::new(Vec::new())),
            reply,
        };
        let app = Router::new()
            .route("/embed/document", post(document))
            .route("/embed/query", post(query))
            .with_state(fake.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{address}"), fake)
    }

    #[tokio::test]
    async fn document_and_query_paths_send_json_and_accept_a_valid_response() {
        let (url, fake) = start_fake_embedder(FakeReply::Success).await;
        let client = EmbedderClient::new(&url, Duration::from_secs(1)).unwrap();

        let document = client
            .embed("stored passage", EmbedKind::StoredPassage)
            .await
            .unwrap();
        let query = client
            .embed("search words", EmbedKind::SearchQuery)
            .await
            .unwrap();

        assert_eq!(document.values.len(), EMBEDDING_DIMENSIONS as usize);
        assert_eq!(document.model_used, "test-embedder");
        assert_eq!(query, document);
        assert_eq!(
            *fake.received.lock().unwrap(),
            [
                ("document", json!({"text": "stored passage"})),
                ("query", json!({"text": "search words"}))
            ]
        );
    }

    #[tokio::test]
    async fn malformed_json_is_reported() {
        let (url, _) = start_fake_embedder(FakeReply::Malformed).await;
        let client = EmbedderClient::new(&url, Duration::from_secs(1)).unwrap();

        let error = client
            .embed("text", EmbedKind::StoredPassage)
            .await
            .unwrap_err();

        assert!(error.contains("cannot read"), "{error}");
    }

    #[tokio::test]
    async fn a_non_success_response_surfaces_the_embedder_error() {
        let (url, _) = start_fake_embedder(FakeReply::Rejected).await;
        let client = EmbedderClient::new(&url, Duration::from_secs(1)).unwrap();

        let error = client
            .embed("text", EmbedKind::StoredPassage)
            .await
            .unwrap_err();

        assert_eq!(error, "configured rejection");
    }

    #[tokio::test]
    async fn timeout_is_reported_distinctly() {
        let (url, _) = start_fake_embedder(FakeReply::Delayed).await;
        let client = EmbedderClient::new(&url, Duration::from_millis(10)).unwrap();

        let error = client
            .embed("text", EmbedKind::SearchQuery)
            .await
            .unwrap_err();

        assert_eq!(error, "the native embedder did not answer in time");
    }

    #[test]
    fn response_constraints_reject_wrong_dimensions_non_finite_values_and_long_model_names() {
        for body in [
            EmbedResponse {
                values: vec![0.0; EMBEDDING_DIMENSIONS as usize - 1],
                model_used: "model".to_owned(),
                truncated: false,
            },
            EmbedResponse {
                values: vec![f32::NAN; EMBEDDING_DIMENSIONS as usize],
                model_used: "model".to_owned(),
                truncated: false,
            },
            EmbedResponse {
                values: vec![0.0; EMBEDDING_DIMENSIONS as usize],
                model_used: "m".repeat(MAX_EMBEDDING_MODEL_CHARS + 1),
                truncated: false,
            },
        ] {
            assert!(validate_response(body).is_err());
        }
    }
}
