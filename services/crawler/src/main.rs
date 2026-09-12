// Crawl job service. Accepts crawl requests over Connect and gRPC, crawls in the background, and reports progress.

mod crawl;
mod entity;
mod extract;
mod jobs;
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
    CrawlScope, CrawlerService, GetCrawlJobRequest, GetCrawlJobResponse, StartCrawlRequest,
    StartCrawlResponse,
};
use connectrpc::{
    ConnectError, RequestContext, Response, Router as ConnectRouter, ServiceRequest, ServiceResult,
};

use sea_orm::Database;

use crate::crawl::Limits;
use crate::jobs::{Jobs, PgJobs, parse_job_id};
use crate::scope::Scope;
use crate::store::PgPages;

const DEFAULT_MAX_PAGES: u32 = 100;
const MAX_PAGES_CEILING: u32 = 10_000;
const MAX_SCOPE_RULES: usize = 100;
const MAX_SCOPE_RULE_CHARS: usize = 2048;
const MAX_IDEMPOTENCY_KEY_CHARS: usize = 128;
const MAX_CONCURRENT_CRAWLS: usize = 4;
const CRAWL_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

struct Crawler {
    jobs: Jobs<PgPages, PgJobs>,
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
        validate_base_url(request.base_url)?;
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
            pages_skipped: job.pages_skipped,
            ..Default::default()
        })
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

fn validate_base_url(raw: &str) -> Result<(), ConnectError> {
    if raw.is_empty() {
        return Err(ConnectError::invalid_argument("base_url is required"));
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
    if is_private_host(&host) {
        return Err(ConnectError::invalid_argument(
            "base_url must point at a public host, not a loopback, private, or link-local address",
        ));
    }
    Ok(())
}

fn is_private_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") || host.ends_with(".localhost") {
        return true;
    }
    let literal = host.trim_start_matches('[').trim_end_matches(']');
    match literal.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => {
            ip.is_loopback()
                || ip.is_private()
                || ip.is_link_local()
                || ip.is_unspecified()
                || ip.is_broadcast()
                || ip.is_multicast()
        }
        Ok(IpAddr::V6(ip)) => {
            ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_multicast()
                || ip.is_unique_local()
                || ip.is_unicast_link_local()
                || ip
                    .to_ipv4_mapped()
                    .is_some_and(|v4| is_private_host(&v4.to_string()))
        }
        Err(_) => false,
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    common::logging::init();
    let max_pages = match std::env::var("CRAWL_MAX_PAGES") {
        Ok(value) => value
            .parse::<u32>()
            .map_err(|error| format!("CRAWL_MAX_PAGES: {error}"))?,
        Err(_) => DEFAULT_MAX_PAGES,
    };
    if max_pages == 0 || max_pages > MAX_PAGES_CEILING {
        return Err(format!("CRAWL_MAX_PAGES must be between 1 and {MAX_PAGES_CEILING}").into());
    }
    let database_url = std::env::var("DATABASE_URL").map_err(|_| "DATABASE_URL is not set")?;
    let db = Database::connect(&database_url).await?;
    db.get_schema_registry("crawler::entity::*")
        .sync(&db)
        .await?;

    let jobs = Jobs::new(
        PgPages::new(db.clone()),
        PgJobs::new(db),
        MAX_CONCURRENT_CRAWLS,
    );
    let interrupted = jobs.fail_unfinished().await?;
    if interrupted > 0 {
        tracing::warn!("marked {interrupted} jobs interrupted by the last restart as failed");
    }

    let crawler = Crawler {
        jobs,
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
    axum::serve(listener, app).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_absolute_http_and_https_urls_on_public_hosts() {
        for url in ["https://example.com", "http://93.184.216.34:8080/docs/"] {
            assert!(validate_base_url(url).is_ok(), "{url}");
        }
    }

    #[test]
    fn rejects_hosts_inside_the_network_the_crawler_runs_in() {
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
            assert!(validate_base_url(url).is_err(), "{url}");
        }
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

    #[test]
    fn rejects_urls_the_crawler_cannot_fetch() {
        for url in [
            "",
            "example.com",
            "/docs",
            "ftp://example.com",
            "https://",
            "not a url",
        ] {
            assert!(validate_base_url(url).is_err(), "{url}");
        }
    }
}
