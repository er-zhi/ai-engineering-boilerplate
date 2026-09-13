// crawler.crawl_jobs: one row per crawl request, so a job survives a restart and a retried request finds its first job.

use common::proto::crawler::v1::CrawlStatus;
use sea_orm::entity::prelude::*;

pub(crate) const MAX_BASE_URL_CHARS: usize = 2048;

#[derive(Clone, Copy, Debug, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "String", db_type = "String(StringLen::N(16))")]
pub enum Status {
    #[sea_orm(string_value = "queued")]
    Queued,
    #[sea_orm(string_value = "running")]
    Running,
    #[sea_orm(string_value = "done")]
    Done,
    #[sea_orm(string_value = "failed")]
    Failed,
}

impl From<Status> for CrawlStatus {
    fn from(status: Status) -> Self {
        match status {
            Status::Queued => Self::Queued,
            Status::Running => Self::Running,
            Status::Done => Self::Done,
            Status::Failed => Self::Failed,
        }
    }
}

impl From<CrawlStatus> for Status {
    fn from(status: CrawlStatus) -> Self {
        match status {
            CrawlStatus::Unspecified | CrawlStatus::Queued => Self::Queued,
            CrawlStatus::Running => Self::Running,
            CrawlStatus::Done => Self::Done,
            CrawlStatus::Failed => Self::Failed,
        }
    }
}

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "crawl_jobs", schema_name = "crawler")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(column_type = "String(StringLen::N(MAX_BASE_URL_CHARS as u32))")]
    pub base_url: String,
    pub status: Status,
    pub pages_crawled: i32,
    pub pages_skipped: i32,
    #[sea_orm(unique, nullable, column_type = "String(StringLen::N(128))")]
    pub idempotency_key: Option<String>,
    pub created_at: DateTimeUtc,
    pub updated_at: DateTimeUtc,
}

impl ActiveModelBehavior for ActiveModel {}
