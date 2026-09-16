use std::sync::Arc;

use connectrpc::Router as ConnectRouter;

use crate::proxy::Gateway;

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

#[path = "knowledge_base.rs"]
mod knowledge_base;
#[path = "crawler.rs"]
mod crawler;
#[path = "chat.rs"]
mod chat;
