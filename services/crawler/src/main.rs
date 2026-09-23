// Crawl job service. Accepts crawl requests over Connect and gRPC, crawls in the background, and reports progress.

mod config;
mod crawl;
mod entity;
mod graph;
mod instance_guard;
mod job_store;
mod jobs;
mod knowledge_base;
mod links;
mod scope;
mod store;
#[cfg(test)]
mod test_db;
#[cfg(test)]
mod test_site;

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::http::Uri;
use axum::routing::get;
use buffa::EnumValue;
use common::proto::crawler::v1::{
    CrawlScope, CrawlerService, GetCrawlJobRequest, GetCrawlJobResponse, GetPageNeighborsRequest,
    GetPageNeighborsResponse, PageNeighbor, PageRelation, StartCrawlRequest, StartCrawlResponse,
};
use connectrpc::{
    ConnectError, RequestContext, Response, Router as ConnectRouter, ServiceRequest, ServiceResult,
};

use sea_orm::{ConnectionTrait, Database};

use crate::crawl::Limits;
use crate::entity::crawl_job::{MAX_BASE_URL_CHARS, STATEMENTS_RUN_AFTER_SCHEMA_SYNC};
use crate::entity::page_edge::RelationType;
use crate::graph::{MAX_REQUESTABLE_DEPTH, PgEdges};
use crate::job_store::PgJobs;
use crate::jobs::{Jobs, MAX_CONCURRENT_CRAWLS, parse_job_id};
use crate::knowledge_base::KnowledgeBaseClient;
use crate::scope::Scope;
use crate::store::PgPages;

const MAX_SCOPE_RULES: usize = 100;
const MAX_SCOPE_RULE_CHARS: usize = 2048;
const MAX_IDEMPOTENCY_KEY_CHARS: usize = 128;
const MAX_REQUESTED_RELATION_TYPES: usize = 4;
const CRAWL_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const KNOWLEDGE_BASE_CALL_TIMEOUT: Duration = Duration::from_secs(90);

struct Crawler {
    jobs: Jobs<PgPages, PgJobs, KnowledgeBaseClient, PgEdges>,
    edges: PgEdges,
    limits: Limits,
}

fn database_failed(error: sea_orm::DbErr) -> ConnectError {
    tracing::error!("database call failed: {error}");
    ConnectError::unavailable("the job store is not available")
}

#[allow(refining_impl_trait)]
impl CrawlerService for Crawler {
    async fn start_crawl(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, StartCrawlRequest>,
    ) -> ServiceResult<StartCrawlResponse> {
        validate_base_url(request.base_url).await?;
        validate_idempotency_key(request.idempotency_key)?;

        let request = request.to_owned_message();
        validate_scope(&request.scope)?;
        let scope = Scope::new(&request.scope);
        let key = (!request.idempotency_key.is_empty()).then_some(request.idempotency_key.as_str());
        let (job, created) = self
            .jobs
            .create(&request.base_url, key)
            .await
            .map_err(database_failed)?;

        if created {
            tokio::spawn({
                let jobs = self.jobs.clone();
                let limits = self.limits;
                let id = job.id;
                async move { jobs.run(id, &request.base_url, scope, limits).await }
            });
        }

        Response::ok(StartCrawlResponse {
            job_id: job.public_id(),
            status: EnumValue::Known(job.status),
            ..Default::default()
        })
    }

    async fn get_crawl_job(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, GetCrawlJobRequest>,
    ) -> ServiceResult<GetCrawlJobResponse> {
        let found = match parse_job_id(request.job_id) {
            Some(id) => self.jobs.get(id).await.map_err(database_failed)?,
            None => None,
        };
        let Some(job) = found else {
            return Err(ConnectError::not_found("unknown job id"));
        };

        Response::ok(GetCrawlJobResponse {
            job_id: request.job_id.to_owned(),
            base_url: job.base_url,
            status: EnumValue::Known(job.status),
            pages_crawled: job.pages_crawled,
            ..Default::default()
        })
    }

    async fn get_page_neighbors(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, GetPageNeighborsRequest>,
    ) -> ServiceResult<GetPageNeighborsResponse> {
        validate_graph_url_length(request.url)?;
        validate_base_url(request.url).await?;
        let max_depth = resolve_max_depth(request.max_depth)?;
        let relation_types = parse_allowed_relation_types(&request.allowed_relation_types)?;

        let neighbors = self
            .edges
            .neighbors(request.url, relation_types, max_depth)
            .await
            .map_err(graph_store_failed)?;

        Response::ok(GetPageNeighborsResponse {
            shallowest_first_neighbors: neighbors.into_iter().map(page_neighbor).collect(),
            ..Default::default()
        })
    }
}

fn graph_store_failed(error: sea_orm::DbErr) -> ConnectError {
    tracing::error!("graph store call failed: {error}");
    ConnectError::unavailable("the graph store is not available")
}

fn validate_graph_url_length(url: &str) -> Result<(), ConnectError> {
    if url.len() > links::MAX_URL_BYTES {
        return Err(ConnectError::invalid_argument(format!(
            "url is longer than {} bytes",
            links::MAX_URL_BYTES
        )));
    }
    Ok(())
}

fn resolve_max_depth(requested: u32) -> Result<u32, ConnectError> {
    match requested {
        0 => Ok(MAX_REQUESTABLE_DEPTH),
        depth if depth > MAX_REQUESTABLE_DEPTH => Err(ConnectError::invalid_argument(format!(
            "max_depth is above {MAX_REQUESTABLE_DEPTH}"
        ))),
        depth => Ok(depth),
    }
}

fn parse_allowed_relation_types(
    relations: &[EnumValue<PageRelation>],
) -> Result<Vec<RelationType>, ConnectError> {
    if relations.len() > MAX_REQUESTED_RELATION_TYPES {
        return Err(ConnectError::invalid_argument(format!(
            "at most {MAX_REQUESTED_RELATION_TYPES} relation types"
        )));
    }
    relations
        .iter()
        .map(|relation| relation_type_from(*relation))
        .collect()
}

fn relation_type_from(relation: EnumValue<PageRelation>) -> Result<RelationType, ConnectError> {
    match relation {
        EnumValue::Known(PageRelation::LinksTo) => Ok(RelationType::LinksTo),
        EnumValue::Known(PageRelation::Parent) => Ok(RelationType::Parent),
        EnumValue::Known(PageRelation::Canonical) => Ok(RelationType::Canonical),
        EnumValue::Known(PageRelation::Redirect) => Ok(RelationType::Redirect),
        _ => Err(ConnectError::invalid_argument(
            "allowed_relation_types must not include PAGE_RELATION_UNSPECIFIED",
        )),
    }
}

fn page_relation_from(relation_type: RelationType) -> PageRelation {
    match relation_type {
        RelationType::LinksTo => PageRelation::LinksTo,
        RelationType::Parent => PageRelation::Parent,
        RelationType::Canonical => PageRelation::Canonical,
        RelationType::Redirect => PageRelation::Redirect,
    }
}

fn page_neighbor(neighbor: crate::graph::Neighbor) -> PageNeighbor {
    PageNeighbor {
        crawled: neighbor.page_id.is_some(),
        url: neighbor.url,
        title: neighbor.title.unwrap_or_default(),
        relation: EnumValue::Known(page_relation_from(neighbor.relation_type)),
        depth: neighbor.depth as u32,
        ..Default::default()
    }
}

fn validate_scope(scope: &CrawlScope) -> Result<(), ConnectError> {
    let rules = scope
        .include_patterns
        .iter()
        .chain(scope.exclude_patterns.iter())
        .chain(scope.include_urls.iter())
        .chain(scope.exclude_urls.iter());
    let mut count = 0;
    for rule in rules {
        count += 1;
        if count > MAX_SCOPE_RULES {
            return Err(ConnectError::invalid_argument(format!(
                "scope holds more than {MAX_SCOPE_RULES} rules"
            )));
        }
        if rule.len() > MAX_SCOPE_RULE_CHARS {
            return Err(ConnectError::invalid_argument(format!(
                "a scope rule is longer than {MAX_SCOPE_RULE_CHARS} characters"
            )));
        }
    }
    Ok(())
}

fn validate_idempotency_key(key: &str) -> Result<(), ConnectError> {
    if key.len() > MAX_IDEMPOTENCY_KEY_CHARS {
        return Err(ConnectError::invalid_argument(format!(
            "idempotency_key is longer than {MAX_IDEMPOTENCY_KEY_CHARS} characters"
        )));
    }
    Ok(())
}

async fn validate_base_url(raw: &str) -> Result<(), ConnectError> {
    if raw.is_empty() {
        return Err(ConnectError::invalid_argument("base_url is required"));
    }
    if raw.chars().count() > MAX_BASE_URL_CHARS {
        return Err(ConnectError::invalid_argument(format!(
            "base_url is longer than {MAX_BASE_URL_CHARS} characters"
        )));
    }
    let Some(host) = raw.parse::<Uri>().ok().and_then(|uri| {
        matches!(uri.scheme_str(), Some("http" | "https"))
            .then(|| uri.host().map(str::to_owned))
            .flatten()
    }) else {
        return Err(ConnectError::invalid_argument(
            "base_url must be an absolute http or https URL",
        ));
    };
    if host.is_empty() {
        return Err(ConnectError::invalid_argument(
            "base_url must be an absolute http or https URL",
        ));
    }
    if host.eq_ignore_ascii_case("localhost") || host.ends_with(".localhost") {
        return Err(ConnectError::invalid_argument(
            "base_url must point at a public host, not a loopback, private, or link-local address",
        ));
    }
    let literal = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = literal.parse::<IpAddr>() {
        return public_ip(ip);
    }

    let addresses = tokio::net::lookup_host((literal, 0))
        .await
        .map_err(|error| {
            ConnectError::invalid_argument(format!("base_url host could not be resolved: {error}"))
        })?;
    let mut resolved_any = false;
    for address in addresses {
        resolved_any = true;
        public_ip(address.ip())?;
    }
    if !resolved_any {
        return Err(ConnectError::invalid_argument(
            "base_url host did not resolve to an address",
        ));
    }
    Ok(())
}

fn public_ip(ip: IpAddr) -> Result<(), ConnectError> {
    if is_private_ip(ip) {
        Err(ConnectError::invalid_argument(
            "base_url must point at a public host, not a loopback, private, or link-local address",
        ))
    } else {
        Ok(())
    }
}

fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            ip.is_loopback()
                || ip.is_private()
                || ip.is_link_local()
                || ip.is_unspecified()
                || ip.is_broadcast()
                || ip.is_multicast()
        }
        IpAddr::V6(ip) => {
            ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_multicast()
                || ip.is_unique_local()
                || ip.is_unicast_link_local()
                || ip
                    .to_ipv4_mapped()
                    .is_some_and(|v4| is_private_ip(IpAddr::V4(v4)))
        }
    }
}

const WORKER_THREADS_BLOCKED_ON_PAGE_BACKPRESSURE: usize =
    MAX_CONCURRENT_CRAWLS * crawl::FETCH_CONCURRENCY;
const FIXED_WORKER_THREADS: usize = 8;
const _: () = assert!(FIXED_WORKER_THREADS > WORKER_THREADS_BLOCKED_ON_PAGE_BACKPRESSURE);

#[tokio::main(worker_threads = 8)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    common::logging::init();
    let config = config::CrawlerConfig::from_env()?;
    let max_pages = config.max_pages();
    let database_url = std::env::var("DATABASE_URL").map_err(|_| "DATABASE_URL is not set")?;
    let db = Database::connect(&database_url).await?;
    let mut instance_guard = instance_guard::CrawlerInstanceGuard::acquire(&db).await?;
    instance_guard.wait_for_previous_owner().await?;
    db.get_schema_registry("crawler::entity::*")
        .sync(&db)
        .await?;
    for statement in STATEMENTS_RUN_AFTER_SCHEMA_SYNC {
        db.execute_unprepared(statement).await?;
    }

    let knowledge_base_url =
        std::env::var("KNOWLEDGE_BASE_URL").unwrap_or_else(|_| "http://127.0.0.1:8084".to_owned());
    let knowledge_base =
        KnowledgeBaseClient::new(&knowledge_base_url, KNOWLEDGE_BASE_CALL_TIMEOUT)?;

    let jobs = Jobs::new(
        PgPages::new(db.clone()),
        PgJobs::new(db.clone()),
        knowledge_base,
        PgEdges::new(db.clone()),
        MAX_CONCURRENT_CRAWLS,
    );
    let interrupted = jobs.fail_unfinished().await?;
    if interrupted > 0 {
        tracing::warn!("marked {interrupted} jobs interrupted by the last restart as failed");
    }

    let crawler = Crawler {
        jobs,
        edges: PgEdges::new(db),
        limits: Limits {
            max_pages,
            request_timeout: CRAWL_REQUEST_TIMEOUT,
        },
    };

    let connect = ConnectRouter::new().add_service(Arc::new(crawler));

    let app = axum::Router::new()
        .route("/health", get(|| async { "OK" }))
        .fallback_service(connect.into_axum_service());

    let listener = tokio::net::TcpListener::bind("0.0.0.0:8081").await?;
    tracing::info!("crawler listening on 0.0.0.0:8081 (max {max_pages} pages per crawl)");
    tokio::select! {
        result = axum::serve(listener, app) => result?,
        result = instance_guard.monitor() => {
            result?;
            return Err("crawler ownership monitor stopped unexpectedly".into());
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::EdgeStore;

    #[tokio::test]
    async fn accepts_absolute_http_and_https_urls_on_public_hosts() {
        for url in ["https://93.184.216.34", "http://93.184.216.34:8080/docs/"] {
            assert!(validate_base_url(url).await.is_ok(), "{url}");
        }
    }

    #[tokio::test]
    async fn rejects_hosts_inside_the_network_the_crawler_runs_in() {
        for url in [
            "http://localhost/",
            "http://app.localhost/",
            "http://127.0.0.1:8080/",
            "http://10.0.0.5/",
            "http://172.16.3.4/",
            "http://192.168.1.1/",
            "http://169.254.169.254/latest/meta-data/",
            "http://0.0.0.0/",
            "http://[::1]/",
            "http://[fd00::1]/",
            "http://[::ffff:127.0.0.1]/",
        ] {
            assert!(validate_base_url(url).await.is_err(), "{url}");
        }
    }

    #[tokio::test]
    async fn rejects_a_hostname_that_dns_resolves_to_loopback() {
        assert!(validate_base_url("http://localhost./").await.is_err());
    }

    #[tokio::test]
    async fn base_url_length_matches_the_storage_column_boundary() {
        let prefix = "http://93.184.216.34/";
        let at_limit = format!("{prefix}{}", "a".repeat(MAX_BASE_URL_CHARS - prefix.len()));
        let over_limit = format!("{at_limit}a");

        assert_eq!(at_limit.chars().count(), MAX_BASE_URL_CHARS);
        assert!(validate_base_url(&at_limit).await.is_ok());
        assert!(validate_base_url(&over_limit).await.is_err());
    }

    #[test]
    fn an_idempotency_key_past_the_limit_is_refused() {
        assert!(validate_idempotency_key(&"k".repeat(MAX_IDEMPOTENCY_KEY_CHARS)).is_ok());
        assert!(validate_idempotency_key(&"k".repeat(MAX_IDEMPOTENCY_KEY_CHARS + 1)).is_err());
    }

    #[test]
    fn a_scope_with_too_many_or_too_long_rules_is_refused() {
        let too_many = CrawlScope {
            include_patterns: vec!["/docs/*".to_owned(); MAX_SCOPE_RULES + 1],
            ..Default::default()
        };
        let too_long = CrawlScope {
            exclude_urls: vec!["x".repeat(MAX_SCOPE_RULE_CHARS + 1)],
            ..Default::default()
        };
        let at_the_limit = CrawlScope {
            include_patterns: vec!["/docs/*".to_owned(); MAX_SCOPE_RULES],
            ..Default::default()
        };

        assert!(validate_scope(&too_many).is_err());
        assert!(validate_scope(&too_long).is_err());
        assert!(validate_scope(&at_the_limit).is_ok());
    }

    #[tokio::test]
    async fn rejects_urls_the_crawler_cannot_fetch() {
        for url in [
            "",
            "example.com",
            "/docs",
            "ftp://example.com",
            "https://",
            "not a url",
        ] {
            assert!(validate_base_url(url).await.is_err(), "{url}");
        }
    }

    #[test]
    fn a_zero_max_depth_resolves_to_the_ceiling() {
        assert_eq!(resolve_max_depth(0).unwrap(), MAX_REQUESTABLE_DEPTH);
    }

    #[test]
    fn a_max_depth_at_the_ceiling_is_accepted() {
        assert_eq!(
            resolve_max_depth(MAX_REQUESTABLE_DEPTH).unwrap(),
            MAX_REQUESTABLE_DEPTH
        );
    }

    #[test]
    fn a_max_depth_above_the_ceiling_is_refused() {
        let error = resolve_max_depth(MAX_REQUESTABLE_DEPTH + 1).unwrap_err();
        assert!(format!("{error:?}").contains("max_depth"), "{error:?}");
    }

    #[test]
    fn an_unspecified_relation_type_is_refused() {
        let error = parse_allowed_relation_types(&[EnumValue::Known(PageRelation::Unspecified)])
            .unwrap_err();
        assert!(
            format!("{error:?}").contains("allowed_relation_types"),
            "{error:?}"
        );
    }

    #[test]
    fn known_relation_types_round_trip_through_the_wire_enum() {
        let requested = [
            EnumValue::Known(PageRelation::LinksTo),
            EnumValue::Known(PageRelation::Parent),
            EnumValue::Known(PageRelation::Canonical),
            EnumValue::Known(PageRelation::Redirect),
        ];

        let parsed = parse_allowed_relation_types(&requested).unwrap();

        assert_eq!(
            parsed,
            [
                RelationType::LinksTo,
                RelationType::Parent,
                RelationType::Canonical,
                RelationType::Redirect,
            ]
        );
    }

    #[test]
    fn more_than_the_relation_type_limit_is_refused() {
        let too_many =
            vec![EnumValue::Known(PageRelation::LinksTo); MAX_REQUESTED_RELATION_TYPES + 1];

        let error = parse_allowed_relation_types(&too_many).unwrap_err();

        assert!(format!("{error:?}").contains("relation types"), "{error:?}");
    }

    #[test]
    fn a_neighbor_with_no_page_id_maps_to_an_uncrawled_response() {
        let neighbor = crate::graph::Neighbor {
            url: "https://example.com/a".to_owned(),
            page_id: None,
            title: None,
            relation_type: RelationType::LinksTo,
            depth: 1,
        };

        let mapped = page_neighbor(neighbor);

        assert!(!mapped.crawled);
        assert_eq!(mapped.title, "");
    }

    #[test]
    fn a_neighbor_with_a_page_id_maps_to_a_crawled_response() {
        let neighbor = crate::graph::Neighbor {
            url: "https://example.com/a".to_owned(),
            page_id: Some(42),
            title: Some("Title".to_owned()),
            relation_type: RelationType::Canonical,
            depth: 2,
        };

        let mapped = page_neighbor(neighbor);

        assert!(mapped.crawled);
        assert_eq!(mapped.title, "Title");
        assert_eq!(mapped.relation, EnumValue::Known(PageRelation::Canonical));
        assert_eq!(mapped.depth, 2);
    }

    use connectrpc::ErrorCode;
    use connectrpc::client::{ClientConfig, HttpClient};

    async fn start_crawler(db: sea_orm::DatabaseConnection) -> String {
        let crawler = Crawler {
            jobs: Jobs::new(
                PgPages::new(db.clone()),
                PgJobs::new(db.clone()),
                KnowledgeBaseClient::new("http://127.0.0.1:1", Duration::from_secs(1)).unwrap(),
                PgEdges::new(db.clone()),
                1,
            ),
            edges: PgEdges::new(db),
            limits: Limits {
                max_pages: 10,
                request_timeout: Duration::from_secs(5),
            },
        };
        let connect = ConnectRouter::new().add_service(Arc::new(crawler));
        let app = axum::Router::new().fallback_service(connect.into_axum_service());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{address}")
    }

    fn client_to(url: &str) -> common::proto::crawler::v1::CrawlerServiceClient<HttpClient> {
        common::proto::crawler::v1::CrawlerServiceClient::new(
            HttpClient::plaintext_http2_only(),
            ClientConfig::new(url.parse().unwrap())
                .with_protocol(connectrpc::Protocol::Grpc)
                .with_default_timeout(Duration::from_secs(5))
                .proto(),
        )
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_page_neighbors_rejects_an_invalid_url_at_the_rpc_boundary() {
        let test = crate::test_db::start().await;
        let url = start_crawler(test.db.clone()).await;
        let client = client_to(&url);

        let error = client
            .get_page_neighbors(GetPageNeighborsRequest {
                url: "not a url".to_owned(),
                ..Default::default()
            })
            .await
            .unwrap_err();

        assert_eq!(error.code, ErrorCode::InvalidArgument);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_page_neighbors_accepts_a_url_at_the_byte_limit_at_the_rpc_boundary() {
        let test = crate::test_db::start().await;
        let url = start_crawler(test.db.clone()).await;
        let client = client_to(&url);
        let long_url = format!("https://example.com/{}", "a".repeat(1024 - 20));
        assert_eq!(long_url.len(), 1024);

        let response = client
            .get_page_neighbors(GetPageNeighborsRequest {
                url: long_url,
                ..Default::default()
            })
            .await
            .unwrap()
            .into_owned();

        assert!(response.shallowest_first_neighbors.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_page_neighbors_rejects_a_url_over_the_byte_limit_at_the_rpc_boundary() {
        let test = crate::test_db::start().await;
        let url = start_crawler(test.db.clone()).await;
        let client = client_to(&url);
        let long_url = format!("https://example.com/{}", "a".repeat(1025 - 20));
        assert_eq!(long_url.len(), 1025);

        let error = client
            .get_page_neighbors(GetPageNeighborsRequest {
                url: long_url,
                ..Default::default()
            })
            .await
            .unwrap_err();

        assert_eq!(error.code, ErrorCode::InvalidArgument);
        assert!(
            error
                .message
                .as_deref()
                .unwrap_or_default()
                .contains("bytes"),
            "expected the length check, not base-URL validation, to reject this: {error:?}"
        );
    }

    #[test]
    fn graph_store_failed_names_the_graph_store_not_the_job_store() {
        let error = graph_store_failed(sea_orm::DbErr::Custom("connection lost".into()));

        assert_eq!(error.code, ErrorCode::Unavailable);
        assert!(
            error
                .message
                .as_deref()
                .unwrap_or_default()
                .contains("graph store"),
            "{error:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_page_neighbors_rejects_a_max_depth_above_the_ceiling_at_the_rpc_boundary() {
        let test = crate::test_db::start().await;
        let url = start_crawler(test.db.clone()).await;
        let client = client_to(&url);

        let error = client
            .get_page_neighbors(GetPageNeighborsRequest {
                url: "https://example.com/a".to_owned(),
                max_depth: MAX_REQUESTABLE_DEPTH + 1,
                ..Default::default()
            })
            .await
            .unwrap_err();

        assert_eq!(error.code, ErrorCode::InvalidArgument);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_page_neighbors_rejects_an_unspecified_relation_type_at_the_rpc_boundary() {
        let test = crate::test_db::start().await;
        let url = start_crawler(test.db.clone()).await;
        let client = client_to(&url);

        let error = client
            .get_page_neighbors(GetPageNeighborsRequest {
                url: "https://example.com/a".to_owned(),
                allowed_relation_types: vec![EnumValue::Known(PageRelation::Unspecified)],
                ..Default::default()
            })
            .await
            .unwrap_err();

        assert_eq!(error.code, ErrorCode::InvalidArgument);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_page_neighbors_returns_a_full_neighbor_from_the_real_store_by_default() {
        let test = crate::test_db::start().await;
        crate::graph::PgEdges::new(test.db.clone())
            .replace_outbound(
                "https://example.com/a",
                vec![crate::links::Link {
                    url: "https://example.com/b".to_owned(),
                    anchor_text: "B".to_owned(),
                }],
            )
            .await
            .unwrap();
        let url = start_crawler(test.db.clone()).await;
        let client = client_to(&url);

        let response = client
            .get_page_neighbors(GetPageNeighborsRequest {
                url: "https://example.com/a".to_owned(),
                ..Default::default()
            })
            .await
            .unwrap()
            .into_owned();

        assert_eq!(response.shallowest_first_neighbors.len(), 1);
        let neighbor = &response.shallowest_first_neighbors[0];
        assert_eq!(neighbor.url, "https://example.com/b");
        assert!(!neighbor.crawled);
        assert_eq!(neighbor.relation, EnumValue::Known(PageRelation::LinksTo));
        assert_eq!(neighbor.depth, 1);
    }
}
