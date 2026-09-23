// Builds the backend clients the passthrough tests drive, and starts a Gateway over them.

use std::sync::Arc;
use std::time::Duration;

use common::proto::chat::v1::ChatServiceClient;
use common::proto::crawler::v1::CrawlerServiceClient;
use common::proto::knowledge_base::v1::KnowledgeBaseServiceClient;
use connectrpc::client::{ClientConfig, HttpClient};
use connectrpc::{Protocol, Router as ConnectRouter};

use crate::proxy::{
    CHAT_CALL_TIMEOUT, CRAWLER_CALL_TIMEOUT, Gateway, KNOWLEDGE_BASE_CALL_TIMEOUT,
};

const UNREACHABLE_URL: &str = "http://127.0.0.1:1";

pub(super) async fn start_gateway(gateway: Gateway) -> String {
    let gateway = Arc::new(gateway);
    let connect = ConnectRouter::new()
        .add_service::<_, common::proto::crawler::v1::CrawlerServiceRegisterMarker>(Arc::clone(
            &gateway,
        ))
        .add_service::<_, common::proto::knowledge_base::v1::KnowledgeBaseServiceRegisterMarker>(
            Arc::clone(&gateway),
        )
        .add_service::<_, common::proto::chat::v1::ChatServiceRegisterMarker>(gateway);
    let app = axum::Router::new().fallback_service(connect.into_axum_service());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{address}")
}

fn client_to<C>(build: impl Fn(HttpClient, ClientConfig) -> C, url: &str, timeout: Duration) -> C {
    build(
        HttpClient::plaintext_http2_only(),
        ClientConfig::new(url.parse().unwrap())
            .with_protocol(Protocol::Grpc)
            .with_default_timeout(timeout)
            .proto(),
    )
}

pub(super) fn crawler_client_to(url: &str) -> CrawlerServiceClient<HttpClient> {
    client_to(CrawlerServiceClient::new, url, CRAWLER_CALL_TIMEOUT)
}

pub(super) fn knowledge_base_client_to(url: &str) -> KnowledgeBaseServiceClient<HttpClient> {
    client_to(
        KnowledgeBaseServiceClient::new,
        url,
        KNOWLEDGE_BASE_CALL_TIMEOUT,
    )
}

pub(super) fn chat_client_to(url: &str, timeout: Duration) -> ChatServiceClient<HttpClient> {
    client_to(ChatServiceClient::new, url, timeout)
}

pub(super) fn unreachable_crawler_client() -> CrawlerServiceClient<HttpClient> {
    crawler_client_to(UNREACHABLE_URL)
}

pub(super) fn unreachable_knowledge_base_client() -> KnowledgeBaseServiceClient<HttpClient> {
    knowledge_base_client_to(UNREACHABLE_URL)
}

pub(super) fn unreachable_chat_client() -> ChatServiceClient<HttpClient> {
    chat_client_to(UNREACHABLE_URL, CHAT_CALL_TIMEOUT)
}

#[path = "knowledge_base.rs"]
mod knowledge_base;
#[path = "crawler.rs"]
mod crawler;
#[path = "chat.rs"]
mod chat;
