// crawler.pages: one row per crawled URL, holding extracted text only, never raw HTML.

use sea_orm::entity::prelude::*;

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "pages", schema_name = "crawler")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(unique, column_type = "String(StringLen::N(2048))")]
    pub url: String,
    #[sea_orm(column_type = "String(StringLen::N(512))")]
    pub title: String,
    #[sea_orm(column_type = "Text")]
    pub main_text: String,
    #[sea_orm(column_type = "Char(Some(64))")]
    pub content_hash: String,
    pub http_status: i16,
    pub crawled_at: DateTimeUtc,
}

impl ActiveModelBehavior for ActiveModel {}
