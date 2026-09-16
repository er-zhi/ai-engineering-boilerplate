// kb_search / kb_read_document: thin wrappers over the knowledge_base.v1.KnowledgeBaseService
// this stack already runs — same client-construction pattern as
// services/knowledge-base/src/llm_router_client.rs uses for llm-router.

use common::proto::knowledge_base::v1::{
    DocumentRef, KnowledgeBaseServiceClient, ReadDocumentRequest, SearchRequest,
};
use connectrpc::Protocol;
use connectrpc::client::{ClientConfig, HttpClient};
use std::time::Duration;

const CALL_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct SearchResultRow {
    pub title: String,
    pub snippet: String,
    pub source: String,
    pub source_id: String,
}

pub struct KnowledgeBaseClient {
    inner: KnowledgeBaseServiceClient<HttpClient>,
}

impl KnowledgeBaseClient {
    pub fn new(url: &str) -> Result<Self, String> {
        let target = url
            .parse()
            .map_err(|e| format!("could not parse knowledge-base URL {url:?}: {e}"))?;
        Ok(Self {
            inner: KnowledgeBaseServiceClient::new(
                HttpClient::plaintext_http2_only(),
                ClientConfig::new(target)
                    .with_protocol(Protocol::Grpc)
                    .with_default_timeout(CALL_TIMEOUT)
                    .proto(),
            ),
        })
    }

    pub async fn search(&self, query: &str, limit: u8) -> Result<Vec<SearchResultRow>, String> {
        let response = self
            .inner
            .search(SearchRequest {
                query: query.to_owned(),
                page_types: vec![],
                limit: u32::from(limit),
                ..Default::default()
            })
            .await
            .map_err(|e| e.to_string())?
            .into_owned();
        Ok(response
            .results
            .into_iter()
            .map(|result| SearchResultRow {
                title: result.title,
                snippet: result.snippet,
                source: result.document.source.clone(),
                source_id: result.document.source_id.clone(),
            })
            .collect())
    }

    pub async fn read_document(
        &self,
        source: &str,
        source_id: &str,
        version: &str,
    ) -> Result<String, String> {
        let response = self
            .inner
            .read_document(ReadDocumentRequest {
                document: DocumentRef {
                    source: source.to_owned(),
                    source_id: source_id.to_owned(),
                    version: version.to_owned(),
                    ..Default::default()
                }
                .into(),
                cursor: String::new(),
                max_chars: 0,
                ..Default::default()
            })
            .await
            .map_err(|e| e.to_string())?
            .into_owned();
        Ok(response.content)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use common::proto::knowledge_base::v1::{
        IngestRequest, IngestResponse, KnowledgeBaseService, ReadDocumentResponse, SearchResponse,
        SearchResult,
    };
    use connectrpc::{
        RequestContext, Response, Router as ConnectRouter, ServiceRequest, ServiceResult,
    };

    use super::*;

    struct FakeKnowledgeBase {
        received_queries: Mutex<Vec<String>>,
    }

    #[allow(refining_impl_trait)]
    impl KnowledgeBaseService for FakeKnowledgeBase {
        async fn search(
            &self,
            _ctx: RequestContext,
            request: ServiceRequest<'_, SearchRequest>,
        ) -> ServiceResult<SearchResponse> {
            let owned = request.to_owned_message();
            self.received_queries
                .lock()
                .expect("lock")
                .push(owned.query.clone());
            Response::ok(SearchResponse {
                results: vec![SearchResult {
                    source: "academy.claude.com".to_owned(),
                    source_id: "/courses/borrow-checker".to_owned(),
                    title: "The Borrow Checker".to_owned(),
                    snippet: "Borrowing lets code use a value without taking ownership.".to_owned(),
                    document: DocumentRef {
                        source: "academy.claude.com".to_owned(),
                        source_id: "/courses/borrow-checker".to_owned(),
                        version: "abc123".to_owned(),
                        ..Default::default()
                    }
                    .into(),
                    ..Default::default()
                }],
                ..Default::default()
            })
        }

        async fn read_document(
            &self,
            _ctx: RequestContext,
            _request: ServiceRequest<'_, ReadDocumentRequest>,
        ) -> ServiceResult<ReadDocumentResponse> {
            Response::ok(ReadDocumentResponse {
                content: "Full document text.".to_owned(),
                ..Default::default()
            })
        }

        async fn ingest(
            &self,
            _ctx: RequestContext,
            _request: ServiceRequest<'_, IngestRequest>,
        ) -> ServiceResult<IngestResponse> {
            Response::ok(IngestResponse::default())
        }
    }

    async fn serve(fake: Arc<FakeKnowledgeBase>) -> String {
        let connect = ConnectRouter::new().add_service(fake);
        let app = axum::Router::new().fallback_service(connect.into_axum_service());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("addr");
        tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
        format!("http://{address}")
    }

    #[tokio::test]
    async fn search_sends_the_query_and_maps_results() {
        let fake = Arc::new(FakeKnowledgeBase {
            received_queries: Mutex::new(Vec::new()),
        });
        let url = serve(Arc::clone(&fake)).await;
        let client = KnowledgeBaseClient::new(&url).expect("client");

        let results = client.search("borrow checker", 5).await.expect("search");

        assert_eq!(
            *fake.received_queries.lock().expect("lock"),
            vec!["borrow checker".to_owned()]
        );
        assert_eq!(
            results,
            vec![SearchResultRow {
                title: "The Borrow Checker".to_owned(),
                snippet: "Borrowing lets code use a value without taking ownership.".to_owned(),
                source: "academy.claude.com".to_owned(),
                source_id: "/courses/borrow-checker".to_owned(),
            }]
        );
    }

    #[tokio::test]
    async fn read_document_returns_the_content() {
        let fake = Arc::new(FakeKnowledgeBase {
            received_queries: Mutex::new(Vec::new()),
        });
        let url = serve(fake).await;
        let client = KnowledgeBaseClient::new(&url).expect("client");

        let content = client
            .read_document("academy.claude.com", "/courses/borrow-checker", "abc123")
            .await
            .expect("read");

        assert_eq!(content, "Full document text.");
    }
}
