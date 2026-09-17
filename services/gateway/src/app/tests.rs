// Checks which routes the app leaves public and which it puts behind a session.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use tower::ServiceExt;

use super::*;
use crate::config::GatewayConfig;
use crate::test_db;

#[tokio::test(flavor = "multi_thread")]
async fn routes_keep_health_public_and_protect_pages_and_rpcs() {
    let test = test_db::start().await;
    let config = GatewayConfig::for_test(std::env::temp_dir());
    let app = routes(&config, test.db.clone());

    let health = app
        .clone()
        .oneshot(Request::get("/health").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(health.status(), StatusCode::OK);

    let page = app
        .clone()
        .oneshot(Request::get("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(page.status(), StatusCode::SEE_OTHER);
    assert_eq!(page.headers()[header::LOCATION], "/login");

    let rpc = app
        .oneshot(
            Request::post("/crawler.v1.CrawlerService/StartCrawl")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(rpc.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(rpc.headers()[header::CONTENT_TYPE], "application/json");
}
