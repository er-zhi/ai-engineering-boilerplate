// Enforces session authentication for Gateway pages and RPCs.

use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response as HttpResponse};

use crate::auth::{COOKIE_NAME, generate_token, hash_token, passwords_match, session_token};
use crate::sessions::PgCacheStore;

const SESSION_SWEEP_INTERVAL: Duration = Duration::from_secs(3_600);
const UNAUTHENTICATED_RPC_BODY: &str = r#"{"code":"unauthenticated","message":"log in first"}"#;
const LOGIN_REJECTED_BODY: &str = "Wrong password. <a href=\"/login\">Try again</a>";

#[derive(Clone)]
pub struct Auth {
    sessions: PgCacheStore,
    password: String,
    session_ttl: Duration,
}

impl Auth {
    pub fn new(sessions: PgCacheStore, password: String, session_ttl: Duration) -> Self {
        Self {
            sessions,
            password,
            session_ttl,
        }
    }
}

pub async fn require_session(
    State(auth): State<Auth>,
    request: Request,
    next: Next,
) -> HttpResponse {
    let Some(token) = session_token(request.headers()) else {
        return unauthenticated(request.uri().path());
    };

    match auth.sessions.get(&hash_token(token)).await {
        Ok(Some(_)) => next.run(request).await,
        Ok(None) => unauthenticated(request.uri().path()),
        Err(error) => {
            tracing::error!("session lookup failed: {error:?}");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "the session store is not available",
            )
                .into_response()
        }
    }
}

fn unauthenticated(path: &str) -> HttpResponse {
    if matches!(path, "/" | "/sources") {
        Redirect::to("/login").into_response()
    } else {
        (
            StatusCode::UNAUTHORIZED,
            [(header::CONTENT_TYPE, "application/json")],
            UNAUTHENTICATED_RPC_BODY,
        )
            .into_response()
    }
}

#[derive(serde::Deserialize)]
pub struct LoginForm {
    password: String,
}

pub async fn login_submit(
    State(auth): State<Auth>,
    axum::Form(form): axum::Form<LoginForm>,
) -> HttpResponse {
    if !passwords_match(&form.password, &auth.password) {
        return (
            StatusCode::UNAUTHORIZED,
            axum::response::Html(LOGIN_REJECTED_BODY),
        )
            .into_response();
    }

    let token = generate_token();
    if let Err(error) = auth
        .sessions
        .set(&hash_token(&token), b"", auth.session_ttl)
        .await
    {
        tracing::error!("could not create a session: {error:?}");
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "could not log in; try again",
        )
            .into_response();
    }

    let cookie = format!(
        "{COOKIE_NAME}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}",
        auth.session_ttl.as_secs()
    );
    (
        StatusCode::SEE_OTHER,
        [
            (header::SET_COOKIE, cookie),
            (header::LOCATION, "/".to_owned()),
        ],
    )
        .into_response()
}

pub async fn logout(State(auth): State<Auth>, headers: HeaderMap) -> HttpResponse {
    if let Some(token) = session_token(&headers)
        && let Err(error) = auth.sessions.delete(&hash_token(token)).await
    {
        tracing::error!("could not delete a session on logout: {error:?}");
    }

    let cleared = format!("{COOKIE_NAME}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0");
    (
        StatusCode::SEE_OTHER,
        [
            (header::SET_COOKIE, cleared),
            (header::LOCATION, "/login".to_owned()),
        ],
    )
        .into_response()
}

pub async fn sweep_expired_sessions_periodically(sessions: PgCacheStore) {
    let mut interval = tokio::time::interval(SESSION_SWEEP_INTERVAL);
    loop {
        interval.tick().await;
        let removed = sessions
            .drop_expired(chrono::Utc::now())
            .await
            .inspect_err(|error| tracing::error!("could not sweep expired sessions: {error}"))
            .unwrap_or_default();
        if removed > 0 {
            tracing::info!("swept {removed} expired sessions");
        }
    }
}

#[cfg(test)]
mod tests;
