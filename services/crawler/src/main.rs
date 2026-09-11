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

use std::sync::Arc;
use std::time::Duration;

use axum::http::Uri;
use axum::routing::get;
use buffa::EnumValue;
use common::proto::crawler::v1::{
    CrawlStatus, CrawlerService, GetCrawlJobRequest, GetCrawlJobResponse, StartCrawlRequest,
    StartCrawlResponse,
};
use connectrpc::{
    ConnectError, RequestContext, Response, Router as ConnectRouter, ServiceRequest, ServiceResult,
};

use sea_orm::Database;

use crate::crawl::Limits;
use crate::jobs::Jobs;
use crate::scope::Scope;
use crate::store::PgPages;

const DEFAULT_MAX_PAGES: u32 = 100;

struct Crawler {
    jobs: Jobs<PgPages>,
    limits: Limits,
}

#[allow(refining_impl_trait)]
impl CrawlerService for Crawler {
    async fn start_crawl(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, StartCrawlRequest>,
    ) -> ServiceResult<StartCrawlResponse> {
        validate_base_url(request.base_url)?;

        let request = request.to_owned_message();
        let scope = Scope::new(&request.scope);
        let job_id = self.jobs.create(&request.base_url);

        tokio::spawn({
            let jobs = self.jobs.clone();
            let job_id = job_id.clone();
            let limits = self.limits;
            async move { jobs.run(&job_id, &request.base_url, scope, limits).await }
        });

        Response::ok(StartCrawlResponse {
            job_id,
            status: EnumValue::Known(CrawlStatus::Queued),
            ..Default::default()
        })
    }

    async fn get_crawl_job(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, GetCrawlJobRequest>,
    ) -> ServiceResult<GetCrawlJobResponse> {
        let Some(job) = self.jobs.get(request.job_id) else {
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

fn validate_base_url(raw: &str) -> Result<(), ConnectError> {
    if raw.is_empty() {
        return Err(ConnectError::invalid_argument("base_url is required"));
    }
    let fetchable = raw.parse::<Uri>().is_ok_and(|uri| {
        matches!(uri.scheme_str(), Some("http" | "https"))
            && uri.host().is_some_and(|host| !host.is_empty())
    });
    if fetchable {
        Ok(())
    } else {
        Err(ConnectError::invalid_argument(
            "base_url must be an absolute http or https URL",
        ))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let max_pages = match std::env::var("CRAWL_MAX_PAGES") {
        Ok(value) => value.parse()?,
        Err(_) => DEFAULT_MAX_PAGES,
    };
    let database_url = std::env::var("DATABASE_URL").map_err(|_| "DATABASE_URL is not set")?;
    let db = Database::connect(&database_url).await?;
    db.get_schema_registry("crawler::entity::*")
        .sync(&db)
        .await?;

    let crawler = Crawler {
        jobs: Jobs::new(PgPages::new(db)),
        limits: Limits {
            max_pages,
            request_timeout: Duration::from_secs(15),
        },
    };

    let connect = ConnectRouter::new().add_service(Arc::new(crawler));

    let app = axum::Router::new()
        .route("/health", get(|| async { "OK" }))
        .fallback_service(connect.into_axum_service());

    let listener = tokio::net::TcpListener::bind("0.0.0.0:8081").await?;
    println!("crawler listening on 0.0.0.0:8081 (max {max_pages} pages per crawl)");
    axum::serve(listener, app).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_absolute_http_and_https_urls() {
        for url in ["https://example.com", "http://127.0.0.1:8080/docs/"] {
            assert!(validate_base_url(url).is_ok(), "{url}");
        }
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
