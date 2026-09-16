use std::sync::Arc;

use common::proto::chat::v1::ChatServiceClient;
use common::proto::knowledge_base::v1::{
    DocumentRef, IngestRequest, IngestResponse, KnowledgeBaseService, KnowledgeBaseServiceClient,
    ReadDocumentRequest, ReadDocumentResponse, SearchRequest, SearchResponse, SearchResult,
};
use connectrpc::client::{ClientConfig, HttpClient};
use connectrpc::{
    ConnectError, ErrorCode, Protocol, RequestContext, Response, Router as ConnectRouter,
    ServiceRequest, ServiceResult,
};

use super::start_gateway;
use crate::proxy::{CHAT_CALL_TIMEOUT, Gateway, KNOWLEDGE_BASE_CALL_TIMEOUT};

struct FakeKnowledgeBase {
    ingested: std::sync::Mutex<Vec<IngestRequest>>,
    searched: std::sync::Mutex<Vec<SearchRequest>>,
    read: std::sync::Mutex<Vec<ReadDocumentRequest>>,
    failure: Option<ErrorCode>,
}

#[allow(refining_impl_trait)]
impl KnowledgeBaseService for FakeKnowledgeBase {
    async fn ingest(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, IngestRequest>,
    ) -> ServiceResult<IngestResponse> {
        self.ingested.lock().unwrap().push(request.to_owned_message());
        if let Some(code) = self.failure {
            return Err(ConnectError::new(code, "configured knowledge-base failure"));
        }
        Response::ok(IngestResponse {
            stored: true,
            skipped: false,
            ..Default::default()
        })
    }

    async fn search(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, SearchRequest>,
    ) -> ServiceResult<SearchResponse> {
        self.searched.lock().unwrap().push(request.to_owned_message());
        if let Some(code) = self.failure {
            return Err(ConnectError::new(code, "configured knowledge-base failure"));
        }
        Response::ok(SearchResponse {
            results: vec![SearchResult {
                source: "crawler".to_owned(),
                source_id: "https://example.com/a".to_owned(),
                title: "Title".to_owned(),
                summary: "Summary".to_owned(),
                page_type: "documentation".to_owned(),
                score: 0.75,
                keywords: vec!["docs".to_owned()],
                snippet: "Matching passage".to_owned(),
                updated_at: "2026-09-13T00:00:00Z".to_owned(),
                ..Default::default()
            }],
            ..Default::default()
        })
    }

    async fn read_document(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, ReadDocumentRequest>,
    ) -> ServiceResult<ReadDocumentResponse> {
        self.read.lock().unwrap().push(request.to_owned_message());
        if let Some(code) = self.failure {
            return Err(ConnectError::new(code, "configured knowledge-base failure"));
        }
        Response::ok(ReadDocumentResponse {
            document: DocumentRef {
                source: "crawler".to_owned(),
                source_id: "https://example.com/a".to_owned(),
                version: "0".repeat(64),
                ..Default::default()
            }
            .into(),
            title: "Title".to_owned(),
            content: "Complete document".to_owned(),
            total_chars: 17,
            ..Default::default()
        })
    }
}

fn fake_knowledge_base(failure: Option<ErrorCode>) -> Arc<FakeKnowledgeBase> {
    Arc::new(FakeKnowledgeBase {
        ingested: std::sync::Mutex::new(Vec::new()),
        searched: std::sync::Mutex::new(Vec::new()),
        read: std::sync::Mutex::new(Vec::new()),
        failure,
    })
}

async fn start_fake_knowledge_base(service: Arc<FakeKnowledgeBase>) -> String {
    let connect = ConnectRouter::new().add_service(service);
    let app = axum::Router::new().fallback_service(connect.into_axum_service());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{address}")
}

fn crawler_client_to(url: &str) -> common::proto::crawler::v1::CrawlerServiceClient<HttpClient> {
    common::proto::crawler::v1::CrawlerServiceClient::new(
        HttpClient::plaintext_http2_only(),
        ClientConfig::new(url.parse().unwrap())
            .with_protocol(Protocol::Grpc)
            .proto(),
    )
}

fn knowledge_base_client_to(url: &str) -> KnowledgeBaseServiceClient<HttpClient> {
    KnowledgeBaseServiceClient::new(
        HttpClient::plaintext_http2_only(),
        ClientConfig::new(url.parse().unwrap())
            .with_protocol(Protocol::Grpc)
            .with_default_timeout(KNOWLEDGE_BASE_CALL_TIMEOUT)
            .proto(),
    )
}

fn unreachable_chat_client() -> ChatServiceClient<HttpClient> {
    ChatServiceClient::new(
        HttpClient::plaintext_http2_only(),
        ClientConfig::new("http://127.0.0.1:1".parse().unwrap())
            .with_protocol(Protocol::Grpc)
            .with_default_timeout(CHAT_CALL_TIMEOUT)
            .proto(),
    )
}

#[tokio::test]
async fn search_forwards_the_complete_request_and_response_through_gateway() {
    let fake = fake_knowledge_base(None);
    let knowledge_base_url = start_fake_knowledge_base(Arc::clone(&fake)).await;
    let gateway = Gateway::new(
        "http://127.0.0.1:1".parse().unwrap(),
        knowledge_base_url.parse().unwrap(),
        "http://127.0.0.1:1".parse().unwrap(),
    );
    let gateway_url = start_gateway(gateway).await;
    let client = knowledge_base_client_to(&gateway_url);
    let search = SearchRequest {
        query: "complete query".to_owned(),
        page_types: vec!["documentation".to_owned()],
        limit: 7,
        ..Default::default()
    };
    let search_response = client.search(search.clone()).await.unwrap().into_owned();
    assert_eq!(*fake.searched.lock().unwrap(), [search]);
    assert_eq!(search_response.results.len(), 1);
    assert_eq!(search_response.results[0].snippet, "Matching passage");
    assert_eq!(search_response.results[0].score, 0.75);
}

#[tokio::test]
async fn search_upstream_error_propagates_through_gateway() {
    let fake = fake_knowledge_base(Some(ErrorCode::InvalidArgument));
    let knowledge_base_url = start_fake_knowledge_base(fake).await;
    let gateway = Gateway {
        crawler: crawler_client_to("http://127.0.0.1:1"),
        knowledge_base: knowledge_base_client_to(&knowledge_base_url),
        chat: unreachable_chat_client(),
    };
    let gateway_url = start_gateway(gateway).await;
    let client = knowledge_base_client_to(&gateway_url);
    let search_error = client.search(SearchRequest::default()).await.unwrap_err();
    assert_eq!(search_error.code, ErrorCode::InvalidArgument);
}

#[tokio::test]
async fn ingest_is_never_forwarded_to_knowledge_base() {
    let fake = fake_knowledge_base(None);
    let knowledge_base_url = start_fake_knowledge_base(Arc::clone(&fake)).await;
    let gateway = Gateway {
        crawler: crawler_client_to("http://127.0.0.1:1"),
        knowledge_base: knowledge_base_client_to(&knowledge_base_url),
        chat: unreachable_chat_client(),
    };
    let gateway_url = start_gateway(gateway).await;
    let client = knowledge_base_client_to(&gateway_url);
    let error = client.ingest(IngestRequest::default()).await.unwrap_err();
    assert_eq!(error.code, ErrorCode::Unimplemented);
    assert!(fake.ingested.lock().unwrap().is_empty());
}

#[tokio::test]
async fn read_document_forwards_the_reference_and_page_options() {
    let fake = fake_knowledge_base(None);
    let knowledge_base_url = start_fake_knowledge_base(Arc::clone(&fake)).await;
    let gateway = Gateway {
        crawler: crawler_client_to("http://127.0.0.1:1"),
        knowledge_base: knowledge_base_client_to(&knowledge_base_url),
        chat: unreachable_chat_client(),
    };
    let gateway_url = start_gateway(gateway).await;
    let client = knowledge_base_client_to(&gateway_url);
    let request = ReadDocumentRequest {
        document: DocumentRef {
            source: "crawler".to_owned(),
            source_id: "https://example.com/a".to_owned(),
            version: "0".repeat(64),
            ..Default::default()
        }
        .into(),
        cursor: "kb1:10".to_owned(),
        max_chars: 8_000,
        ..Default::default()
    };

    let response = client
        .read_document(request.clone())
        .await
        .unwrap()
        .into_owned();

    assert_eq!(*fake.read.lock().unwrap(), [request]);
    assert_eq!(response.content, "Complete document");
}
