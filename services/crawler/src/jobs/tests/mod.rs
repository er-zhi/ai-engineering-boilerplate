// Groups crawl-job tests by behavior while sharing focused test doubles.

use std::time::Duration;

use chrono::Utc;
use common::proto::crawler::v1::CrawlScope;
use sea_orm::EntityTrait;

use super::*;
use crate::crawl::CountedPage;
use crate::entity::crawl_job::{self, Status};
use crate::entity::page;
use crate::job_store::{PgJobs, job_from};
use crate::store::{PageStore, PgPages};
use crate::test_db;
use crate::test_site::{TestSite, unreachable_url};

mod fixtures;
mod graph;
mod identity;
mod knowledge_base;
mod lifecycle;
mod persistence;
mod storage;

use fixtures::*;

fn only_docs() -> Scope {
    Scope::new(&CrawlScope {
        include_patterns: vec!["/docs/*".into()],
        ..Default::default()
    })
}

const LIMITS: Limits = Limits {
    max_pages: 50,
    request_timeout: Duration::from_secs(5),
};
const ONE_AT_A_TIME: usize = 1;

async fn job(
    jobs: &Jobs<impl PageStore, impl JobStore, impl KnowledgeBase, impl EdgeStore>,
    id: i64,
) -> CrawlJob {
    jobs.get(id).await.unwrap().unwrap()
}
