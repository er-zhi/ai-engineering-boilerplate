// Persists counted pages: extracts the main text, then inserts or updates the row for the URL.
#![cfg_attr(not(test), allow(dead_code))]

use chrono::Utc;
use sea_orm::sea_query::OnConflict;
use sea_orm::{ActiveValue::Set, DatabaseConnection, DbErr, EntityTrait};

use crate::crawl::CountedPage;
use crate::entity::page;
use crate::extract::extract;

pub trait PageStore: Clone + Send + Sync + 'static {
    fn save(&self, page: CountedPage) -> impl Future<Output = Result<(), DbErr>> + Send;
}

#[derive(Clone)]
pub struct PgPages {
    db: DatabaseConnection,
}

impl PgPages {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }
}

impl PageStore for PgPages {
    async fn save(&self, page: CountedPage) -> Result<(), DbErr> {
        let extracted = extract(&page.url, &page.html);
        let row = page::ActiveModel {
            url: Set(page.url),
            title: Set(extracted.title),
            main_text: Set(extracted.main_text),
            content_hash: Set(extracted.content_hash),
            http_status: Set(i16::try_from(page.status).expect("HTTP status codes fit in smallint")),
            crawled_at: Set(Utc::now()),
            ..Default::default()
        };

        page::Entity::insert(row)
            .on_conflict(
                OnConflict::column(page::Column::Url)
                    .update_columns([
                        page::Column::Title,
                        page::Column::MainText,
                        page::Column::ContentHash,
                        page::Column::HttpStatus,
                        page::Column::CrawledAt,
                    ])
                    .to_owned(),
            )
            .exec(&self.db)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_db;

    fn counted(url: &str, title: &str) -> CountedPage {
        CountedPage {
            url: url.to_owned(),
            html: format!("<html><head><title>{title}</title></head><body></body></html>"),
            status: 200,
        }
    }

    fn valid_row(url: &str) -> page::ActiveModel {
        page::ActiveModel {
            url: Set(url.to_owned()),
            title: Set("Title".to_owned()),
            main_text: Set(String::new()),
            content_hash: Set("0".repeat(64)),
            http_status: Set(200),
            crawled_at: Set(Utc::now()),
            ..Default::default()
        }
    }

    async fn insert(db: &DatabaseConnection, row: page::ActiveModel) -> Result<(), DbErr> {
        page::Entity::insert(row).exec(db).await.map(|_| ())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn database_enforces_the_column_rules_from_the_entity() {
        let test = test_db::start().await;
        let db = &test.db;

        insert(db, valid_row("https://example.com/a"))
            .await
            .unwrap();

        assert!(
            insert(db, valid_row("https://example.com/a"))
                .await
                .is_err(),
            "duplicate url accepted"
        );
        let long_title = page::ActiveModel {
            title: Set("t".repeat(513)),
            ..valid_row("https://example.com/b")
        };
        assert!(
            insert(db, long_title).await.is_err(),
            "513-character title accepted"
        );
        let long_url = valid_row(&format!("https://example.com/{}", "u".repeat(2049)));
        assert!(
            insert(db, long_url).await.is_err(),
            "url over 2048 characters accepted"
        );
        let long_hash = page::ActiveModel {
            content_hash: Set("0".repeat(65)),
            ..valid_row("https://example.com/c")
        };
        assert!(
            insert(db, long_hash).await.is_err(),
            "65-character hash accepted"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn storing_a_url_again_updates_its_row() {
        let test = test_db::start().await;
        let store = PgPages::new(test.db.clone());

        store
            .save(counted("https://example.com/a", "First"))
            .await
            .unwrap();
        store
            .save(counted("https://example.com/a", "Second"))
            .await
            .unwrap();

        let rows = page::Entity::find().all(&test.db).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].title, "Second");
        assert_eq!(rows[0].http_status, 200);
        assert_eq!(rows[0].content_hash.len(), 64);
    }
}
