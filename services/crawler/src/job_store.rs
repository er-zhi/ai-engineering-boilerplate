// Persists crawl job state in the crawler schema.

use chrono::Utc;
use common::proto::crawler::v1::CrawlStatus;
use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::{Expr, ExprTrait};
use sea_orm::{ActiveModelTrait, ColumnTrait, DatabaseConnection, DbErr, EntityTrait, QueryFilter};

use crate::entity::crawl_job::{self, Status};
use crate::jobs::{CrawlJob, JobStore};

#[derive(Clone)]
pub struct PgJobs {
    db: DatabaseConnection,
}

impl PgJobs {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }
}

pub(crate) fn job_from(row: crawl_job::Model) -> CrawlJob {
    CrawlJob {
        id: row.id,
        base_url: row.base_url,
        status: row.status.into(),
        pages_crawled: stored_page_count(row.id, row.pages_crawled),
    }
}

fn stored_page_count(job_id: i64, value: i32) -> u32 {
    u32::try_from(value).unwrap_or_else(|error| {
        tracing::error!(
            job = job_id,
            column = "pages_crawled",
            value,
            "invalid crawl job counter: {error}"
        );
        0
    })
}

impl JobStore for PgJobs {
    async fn create(
        &self,
        base_url: &str,
        idempotency_key: Option<&str>,
    ) -> Result<CrawlJob, DbErr> {
        let now = Utc::now();
        let row = crawl_job::ActiveModel {
            base_url: Set(base_url.to_owned()),
            status: Set(Status::Queued),
            pages_crawled: Set(0),
            idempotency_key: Set(idempotency_key.map(str::to_owned)),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(&self.db)
        .await?;
        Ok(job_from(row))
    }

    async fn find_by_key(&self, key: &str) -> Result<Option<CrawlJob>, DbErr> {
        crawl_job::Entity::find()
            .filter(crawl_job::Column::IdempotencyKey.eq(key))
            .one(&self.db)
            .await
            .map(|row| row.map(job_from))
    }

    async fn get(&self, id: i64) -> Result<Option<CrawlJob>, DbErr> {
        crawl_job::Entity::find_by_id(id)
            .one(&self.db)
            .await
            .map(|row| row.map(job_from))
    }

    async fn set_status(&self, id: i64, status: CrawlStatus) -> Result<(), DbErr> {
        crawl_job::Entity::update_many()
            .col_expr(crawl_job::Column::Status, Expr::value(Status::from(status)))
            .col_expr(crawl_job::Column::UpdatedAt, Expr::value(Utc::now()))
            .filter(crawl_job::Column::Id.eq(id))
            .exec(&self.db)
            .await
            .map(|_| ())
    }

    async fn add_crawled_page(&self, id: i64) -> Result<(), DbErr> {
        crawl_job::Entity::update_many()
            .col_expr(
                crawl_job::Column::PagesCrawled,
                Expr::col(crawl_job::Column::PagesCrawled).add(1),
            )
            .col_expr(crawl_job::Column::UpdatedAt, Expr::value(Utc::now()))
            .filter(crawl_job::Column::Id.eq(id))
            .exec(&self.db)
            .await
            .map(|_| ())
    }

    async fn fail_unfinished(&self) -> Result<u64, DbErr> {
        crawl_job::Entity::update_many()
            .col_expr(crawl_job::Column::Status, Expr::value(Status::Failed))
            .col_expr(crawl_job::Column::UpdatedAt, Expr::value(Utc::now()))
            .filter(crawl_job::Column::Status.is_in([Status::Queued, Status::Running]))
            .exec(&self.db)
            .await
            .map(|updated| updated.rows_affected)
    }
}
