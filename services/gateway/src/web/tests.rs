use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::middleware;
use axum::routing::get;
use common::cache::CacheStore;
use tower::ServiceExt;

use super::*;
use crate::auth::{COOKIE_NAME, hash_token};
use crate::test_db;

async fn auth() -> (Auth, common::test_db::TestDb) {
    let test = test_db::start().await;
    let sessions = PgCacheStore::new(test.db.clone());
    (
        Auth::new(sessions, "secret".to_owned(), Duration::from_secs(60)),
        test,
    )
}

fn protected(auth: Auth) -> axum::Router {
    axum::Router::new()
        .route("/private", get(|| async { "welcome" }))
        .layer(middleware::from_fn_with_state(auth, require_session))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_valid_login_creates_a_session_that_authorizes_then_logout_deletes_it() {
    let (auth, _test) = auth().await;
    let login = login_submit(
        State(auth.clone()),
        axum::Form(LoginForm {
            password: "secret".to_owned(),
        }),
    )
    .await;

    assert_eq!(login.status(), StatusCode::SEE_OTHER);
    assert_eq!(login.headers()[header::LOCATION], "/");
    let cookie = login.headers()[header::SET_COOKIE].to_str().unwrap();
    let token = cookie_value(cookie, COOKIE_NAME).unwrap();
    assert_eq!(
        auth.sessions.get(&hash_token(token)).await.unwrap(),
        Some(Vec::new())
    );

    let response = protected(auth.clone())
        .oneshot(
            Request::get("/private")
                .header(header::COOKIE, format!("{COOKIE_NAME}={token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let mut headers = HeaderMap::new();
    headers.insert(
        header::COOKIE,
        format!("{COOKIE_NAME}={token}").parse().unwrap(),
    );
    let response = logout(State(auth.clone()), headers).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(response.headers()[header::LOCATION], "/login");
    assert_eq!(auth.sessions.get(&hash_token(token)).await.unwrap(), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn login_rejects_a_wrong_password_without_creating_a_cookie() {
    let (auth, _test) = auth().await;
    let response = login_submit(
        State(auth),
        axum::Form(LoginForm {
            password: "wrong".to_owned(),
        }),
    )
    .await;

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(response.headers().get(header::SET_COOKIE).is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_and_unknown_sessions_are_rejected_by_route_kind() {
    let (auth, _test) = auth().await;
    let missing = protected(auth.clone())
        .oneshot(Request::get("/private").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::UNAUTHORIZED);

    let unknown = protected(auth)
        .oneshot(
            Request::get("/private")
                .header(header::COOKIE, format!("{COOKIE_NAME}=unknown"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unknown.status(), StatusCode::UNAUTHORIZED);

    let page = unauthenticated("/sources");
    assert_eq!(page.status(), StatusCode::SEE_OTHER);
    assert_eq!(page.headers()[header::LOCATION], "/login");
}

#[tokio::test(flavor = "multi_thread")]
async fn logout_without_a_cookie_is_still_idempotent() {
    let (auth, _test) = auth().await;
    let response = logout(State(auth), HeaderMap::new()).await;

    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(response.headers()[header::LOCATION], "/login");
    assert!(
        response.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .ends_with("Max-Age=0")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_periodic_sweep_removes_expired_sessions_on_its_first_tick() {
    let (auth, _test) = auth().await;
    auth.sessions
        .set("expired", b"", Duration::from_millis(1))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;

    let task = tokio::spawn(sweep_expired_sessions_periodically(auth.sessions.clone()));
    tokio::time::sleep(Duration::from_millis(50)).await;
    task.abort();

    assert_eq!(
        auth.sessions
            .drop_expired(chrono::Utc::now())
            .await
            .unwrap(),
        0
    );
}
