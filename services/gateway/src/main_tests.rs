// Exercises Gateway proxy forwarding, retries, and error propagation.

    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use buffa::EnumValue;
    use common::proto::crawler::v1::{
        CrawlerService, CrawlerServiceClient, GetCrawlJobRequest, GetCrawlJobResponse,
        GetPageNeighborsRequest, GetPageNeighborsResponse, PageNeighbor, PageRelation,
        StartCrawlRequest, StartCrawlResponse,
    };
    use common::proto::knowledge_base::v1::{
        IngestRequest, IngestResponse, KnowledgeBaseService, KnowledgeBaseServiceClient,
        SearchRequest, SearchResponse, SearchResult,
    };
    use connectrpc::client::{ClientConfig, HttpClient};
    use connectrpc::{
        ConnectError, ErrorCode, Protocol, RequestContext, Response, Router as ConnectRouter,
        ServiceRequest, ServiceResult,
    };

    use crate::proxy::{CRAWLER_CALL_TIMEOUT, Gateway, KNOWLEDGE_BASE_CALL_TIMEOUT};

    struct FakeKnowledgeBase {
        ingested: std::sync::Mutex<Vec<IngestRequest>>,
        searched: std::sync::Mutex<Vec<SearchRequest>>,
        failure: Option<ErrorCode>,
    }

    #[allow(refining_impl_trait)]
    impl KnowledgeBaseService for FakeKnowledgeBase {
        async fn ingest(
            &self,
            _ctx: RequestContext,
            request: ServiceRequest<'_, IngestRequest>,
        ) -> ServiceResult<IngestResponse> {
            self.ingested
                .lock()
                .unwrap()
                .push(request.to_owned_message());
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
            self.searched
                .lock()
                .unwrap()
                .push(request.to_owned_message());
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
    }

    fn fake_knowledge_base(failure: Option<ErrorCode>) -> Arc<FakeKnowledgeBase> {
        Arc::new(FakeKnowledgeBase {
            ingested: std::sync::Mutex::new(Vec::new()),
            searched: std::sync::Mutex::new(Vec::new()),
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

    struct FakeCrawler {
        received: std::sync::Mutex<Option<GetPageNeighborsRequest>>,
        neighbor_attempts: AtomicUsize,
        neighbor_failures_remaining: AtomicUsize,
        neighbor_failure_code: ErrorCode,
        start_crawl_attempts: AtomicUsize,
        start_crawl_failure_code: Option<ErrorCode>,
    }

    #[allow(refining_impl_trait)]
    impl CrawlerService for FakeCrawler {
        async fn start_crawl(
            &self,
            _ctx: RequestContext,
            _request: ServiceRequest<'_, StartCrawlRequest>,
        ) -> ServiceResult<StartCrawlResponse> {
            self.start_crawl_attempts.fetch_add(1, Ordering::Relaxed);
            if let Some(code) = self.start_crawl_failure_code {
                return Err(ConnectError::new(code, "configured fake crawler failure"));
            }
            Response::ok(StartCrawlResponse::default())
        }

        async fn get_crawl_job(
            &self,
            _ctx: RequestContext,
            _request: ServiceRequest<'_, GetCrawlJobRequest>,
        ) -> ServiceResult<GetCrawlJobResponse> {
            Response::ok(GetCrawlJobResponse::default())
        }

        async fn get_page_neighbors(
            &self,
            _ctx: RequestContext,
            request: ServiceRequest<'_, GetPageNeighborsRequest>,
        ) -> ServiceResult<GetPageNeighborsResponse> {
            self.neighbor_attempts.fetch_add(1, Ordering::Relaxed);
            *self.received.lock().unwrap() = Some(request.to_owned_message());
            if self
                .neighbor_failures_remaining
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_ok()
            {
                return Err(ConnectError::new(
                    self.neighbor_failure_code,
                    "configured fake crawler failure",
                ));
            }

            Response::ok(GetPageNeighborsResponse {
                shallowest_first_neighbors: vec![PageNeighbor {
                    crawled: true,
                    url: "https://example.com/found".to_owned(),
                    title: "Found".to_owned(),
                    relation: EnumValue::Known(PageRelation::Canonical),
                    depth: 2,
                    ..Default::default()
                }],
                ..Default::default()
            })
        }
    }

    fn fake_crawler(failures: usize, failure_code: ErrorCode) -> Arc<FakeCrawler> {
        Arc::new(FakeCrawler {
            received: std::sync::Mutex::new(None),
            neighbor_attempts: AtomicUsize::new(0),
            neighbor_failures_remaining: AtomicUsize::new(failures),
            neighbor_failure_code: failure_code,
            start_crawl_attempts: AtomicUsize::new(0),
            start_crawl_failure_code: None,
        })
    }

    async fn start_fake_crawler(service: Arc<FakeCrawler>) -> String {
        let connect = ConnectRouter::new().add_service(service);
        let app = axum::Router::new().fallback_service(connect.into_axum_service());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{address}")
    }

    async fn start_gateway(gateway: Gateway) -> String {
        let gateway = Arc::new(gateway);
        let connect = ConnectRouter::new()
            .add_service::<_, common::proto::crawler::v1::CrawlerServiceRegisterMarker>(
                Arc::clone(&gateway),
            )
            .add_service::<_, common::proto::knowledge_base::v1::KnowledgeBaseServiceRegisterMarker>(
                gateway,
            );
        let app = axum::Router::new().fallback_service(connect.into_axum_service());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{address}")
    }

    fn client_to(url: &str) -> CrawlerServiceClient<HttpClient> {
        CrawlerServiceClient::new(
            HttpClient::plaintext_http2_only(),
            ClientConfig::new(url.parse().unwrap())
                .with_protocol(Protocol::Grpc)
                .with_default_timeout(CRAWLER_CALL_TIMEOUT)
                .proto(),
        )
    }

    fn unreachable_knowledge_base_client() -> KnowledgeBaseServiceClient<HttpClient> {
        KnowledgeBaseServiceClient::new(
            HttpClient::plaintext_http2_only(),
            ClientConfig::new("http://127.0.0.1:1".parse().unwrap())
                .with_protocol(Protocol::Grpc)
                .with_default_timeout(KNOWLEDGE_BASE_CALL_TIMEOUT)
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

    #[tokio::test]
    async fn search_forwards_the_complete_request_and_response_through_gateway() {
        let fake = fake_knowledge_base(None);
        let knowledge_base_url = start_fake_knowledge_base(Arc::clone(&fake)).await;
        let gateway = Gateway {
            crawler: client_to("http://127.0.0.1:1"),
            knowledge_base: knowledge_base_client_to(&knowledge_base_url),
        };
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
            crawler: client_to("http://127.0.0.1:1"),
            knowledge_base: knowledge_base_client_to(&knowledge_base_url),
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
            crawler: client_to("http://127.0.0.1:1"),
            knowledge_base: knowledge_base_client_to(&knowledge_base_url),
        };
        let gateway_url = start_gateway(gateway).await;
        let client = knowledge_base_client_to(&gateway_url);

        let error = client.ingest(IngestRequest::default()).await.unwrap_err();

        assert_eq!(error.code, ErrorCode::Unimplemented);
        assert!(fake.ingested.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn get_page_neighbors_forwards_the_complete_request_and_response() {
        let fake_crawler = fake_crawler(0, ErrorCode::Unavailable);
        let crawler_url = start_fake_crawler(Arc::clone(&fake_crawler)).await;
        let gateway = Gateway {
            crawler: client_to(&crawler_url),
            knowledge_base: unreachable_knowledge_base_client(),
        };
        let gateway_url = start_gateway(gateway).await;
        let client = client_to(&gateway_url);

        let sent = GetPageNeighborsRequest {
            url: "https://example.com/start".to_owned(),
            allowed_relation_types: vec![
                EnumValue::Known(PageRelation::LinksTo),
                EnumValue::Known(PageRelation::Canonical),
            ],
            max_depth: 2,
            ..Default::default()
        };

        let response = client
            .get_page_neighbors(sent.clone())
            .await
            .unwrap()
            .into_owned();

        assert_eq!(*fake_crawler.received.lock().unwrap(), Some(sent));
        assert_eq!(response.shallowest_first_neighbors.len(), 1);
        let neighbor = &response.shallowest_first_neighbors[0];
        assert!(neighbor.crawled);
        assert_eq!(neighbor.url, "https://example.com/found");
        assert_eq!(neighbor.title, "Found");
        assert_eq!(neighbor.relation, EnumValue::Known(PageRelation::Canonical));
        assert_eq!(neighbor.depth, 2);
    }

    #[tokio::test]
    async fn get_page_neighbors_retries_transient_failures_until_success() {
        let fake_crawler = fake_crawler(2, ErrorCode::Unavailable);
        let crawler_url = start_fake_crawler(Arc::clone(&fake_crawler)).await;
        let gateway = Gateway {
            crawler: client_to(&crawler_url),
            knowledge_base: unreachable_knowledge_base_client(),
        };
        let gateway_url = start_gateway(gateway).await;
        let client = client_to(&gateway_url);

        let response = client
            .get_page_neighbors(GetPageNeighborsRequest::default())
            .await
            .unwrap()
            .into_owned();

        assert_eq!(fake_crawler.neighbor_attempts.load(Ordering::Relaxed), 3);
        assert_eq!(response.shallowest_first_neighbors.len(), 1);
    }

    #[tokio::test]
    async fn get_page_neighbors_does_not_retry_permanent_failures() {
        let fake_crawler = fake_crawler(1, ErrorCode::InvalidArgument);
        let crawler_url = start_fake_crawler(Arc::clone(&fake_crawler)).await;
        let gateway = Gateway {
            crawler: client_to(&crawler_url),
            knowledge_base: unreachable_knowledge_base_client(),
        };
        let gateway_url = start_gateway(gateway).await;
        let client = client_to(&gateway_url);

        let error = client
            .get_page_neighbors(GetPageNeighborsRequest::default())
            .await
            .unwrap_err();

        assert_eq!(error.code, ErrorCode::InvalidArgument);
        assert_eq!(fake_crawler.neighbor_attempts.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn start_crawl_is_never_retried_even_on_a_transient_failure() {
        let fake_crawler = Arc::new(FakeCrawler {
            received: std::sync::Mutex::new(None),
            neighbor_attempts: AtomicUsize::new(0),
            neighbor_failures_remaining: AtomicUsize::new(0),
            neighbor_failure_code: ErrorCode::Unavailable,
            start_crawl_attempts: AtomicUsize::new(0),
            start_crawl_failure_code: Some(ErrorCode::Unavailable),
        });
        let crawler_url = start_fake_crawler(Arc::clone(&fake_crawler)).await;
        let gateway = Gateway {
            crawler: client_to(&crawler_url),
            knowledge_base: unreachable_knowledge_base_client(),
        };
        let gateway_url = start_gateway(gateway).await;
        let client = client_to(&gateway_url);

        let error = client
            .start_crawl(StartCrawlRequest::default())
            .await
            .unwrap_err();

        assert_eq!(error.code, ErrorCode::Unavailable);
        assert_eq!(fake_crawler.start_crawl_attempts.load(Ordering::Relaxed), 1);
    }
