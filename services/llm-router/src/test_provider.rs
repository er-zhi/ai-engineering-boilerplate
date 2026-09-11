// Test-only provider on 127.0.0.1 that answers the OpenAI chat-completions path with a scripted reply and records what it was sent.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Json;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use serde_json::{Value, json};

#[derive(Clone, Debug)]
pub struct Seen {
    pub authorization: String,
    pub body: Value,
}

pub struct TestProvider {
    base_url: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl TestProvider {
    pub async fn answering(status: StatusCode, reply: Value) -> Self {
        Self::start(status, reply, Duration::ZERO, StatusCode::OK).await
    }

    pub async fn answering_after(status: StatusCode, reply: Value, delay: Duration) -> Self {
        Self::start(status, reply, delay, StatusCode::OK).await
    }

    pub async fn answering_key(key_status: StatusCode) -> Self {
        Self::start(StatusCode::OK, json!({}), Duration::ZERO, key_status).await
    }

    async fn start(
        status: StatusCode,
        reply: Value,
        delay: Duration,
        key_status: StatusCode,
    ) -> Self {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let reply = Arc::new(reply);

        let app = axum::Router::new()
            .route(
                "/chat/completions",
                post({
                    let seen = seen.clone();
                    move |headers: HeaderMap, Json(body): Json<Value>| {
                        let seen = seen.clone();
                        let reply = reply.clone();
                        async move {
                            seen.lock().unwrap().push(Seen {
                                authorization: bearer(&headers),
                                body,
                            });
                            tokio::time::sleep(delay).await;
                            (status, Json(reply.as_ref().clone()))
                        }
                    }
                }),
            )
            .route(
                "/key",
                get(move || async move { (key_status, Json(json!({"data": {"label": "test"}}))) }),
            );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        Self { base_url, seen }
    }

    pub fn base_url(&self) -> String {
        self.base_url.clone()
    }

    pub fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

fn bearer(headers: &HeaderMap) -> String {
    headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned()
}
