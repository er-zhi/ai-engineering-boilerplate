// Persists counted pages: extracts the main text, then inserts or updates the row for the URL, reporting whether the content actually changed.

use chrono::Utc;
use sea_orm::sea_query::OnConflict;
use sea_orm::{ActiveValue::Set, ColumnTrait, DatabaseConnection, DbErr, EntityTrait, QueryFilter};

use crate::crawl::CountedPage;
use crate::entity::page;
use common::extract::extract;

pub struct Saved {
    pub title: String,
    pub main_text: String,
    pub changed: bool,
}

pub trait PageStore: Clone + Send + Sync + 'static {
    fn save(&self, page: CountedPage) -> impl Future<Output = Result<Saved, DbErr>> + Send;
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
    async fn save(&self, page: CountedPage) -> Result<Saved, DbErr> {
        let extracted = extract(&page.url, &page.html);

        let previous_hash = page::Entity::find()
            .filter(page::Column::Url.eq(&page.url))
            .one(&self.db)
            .await?
            .map(|row| row.content_hash);
        let changed = previous_hash.as_deref() != Some(extracted.content_hash.as_str());

        let row = page::ActiveModel {
            url: Set(page.url),
            title: Set(extracted.title.clone()),
            main_text: Set(extracted.main_text.clone()),
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

        Ok(Saved {
            title: extracted.title,
            main_text: extracted.main_text,
            changed,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_db;

    fn counted(url: &str, title: &str) -> CountedPage {
        counted_with_body(url, title, "")
    }

    fn counted_with_body(url: &str, title: &str, body: &str) -> CountedPage {
        CountedPage {
            url: url.to_owned(),
            html: format!("<html><head><title>{title}</title></head><body>{body}</body></html>"),
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

    #[tokio::test(flavor = "multi_thread")]
    async fn a_first_save_is_reported_as_changed() {
        let test = test_db::start().await;
        let store = PgPages::new(test.db.clone());

        let saved = store
            .save(counted("https://example.com/a", "First"))
            .await
            .unwrap();

        assert!(saved.changed);
        assert_eq!(saved.title, "First");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn resaving_identical_content_is_reported_as_unchanged() {
        let test = test_db::start().await;
        let store = PgPages::new(test.db.clone());

        store
            .save(counted("https://example.com/a", "Same"))
            .await
            .unwrap();
        let resaved = store
            .save(counted("https://example.com/a", "Same"))
            .await
            .unwrap();

        assert!(!resaved.changed);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn resaving_with_different_content_is_reported_as_changed() {
        let test = test_db::start().await;
        let store = PgPages::new(test.db.clone());

        store
            .save(counted_with_body(
                "https://example.com/a",
                "Title",
                "<p>First version of the article.</p>",
            ))
            .await
            .unwrap();
        let resaved = store
            .save(counted_with_body(
                "https://example.com/a",
                "Title",
                "<p>Second version of the article.</p>",
            ))
            .await
            .unwrap();

        assert!(resaved.changed);
    }
}
