// knowledge_base.documents: one row per (source, source_id), holding the full content and its LLM-derived page type, keywords, and summary.

use sea_orm::entity::prelude::*;

const MAX_KEYWORD_CHARS: u32 = 64;
pub const MAX_EMBEDDING_MODEL_CHARS: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "i16", db_type = "SmallInteger")]
pub enum PageType {
    #[sea_orm(num_value = 0)]
    Other,
    #[sea_orm(num_value = 1)]
    Product,
    #[sea_orm(num_value = 2)]
    Knowledge,
    #[sea_orm(num_value = 3)]
    Instruction,
    #[sea_orm(num_value = 4)]
    Documentation,
    #[sea_orm(num_value = 5)]
    Blog,
}

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "documents", schema_name = "knowledge_base")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(
        unique_key = "source_and_source_id",
        column_type = "String(StringLen::N(64))"
    )]
    pub source: String,
    #[sea_orm(
        unique_key = "source_and_source_id",
        column_type = "String(StringLen::N(2048))"
    )]
    pub source_id: String,
    #[sea_orm(column_type = "String(StringLen::N(512))")]
    pub title: String,
    #[sea_orm(column_type = "Text")]
    pub content: String,
    #[sea_orm(column_type = "Char(Some(64))")]
    pub content_hash: String,
    pub page_type: PageType,
    #[sea_orm(
        column_type = "Array(RcOrArc::new(ColumnType::String(StringLen::N(MAX_KEYWORD_CHARS))))"
    )]
    pub keywords: Vec<String>,
    #[sea_orm(column_type = "Text")]
    pub summary: String,
    #[sea_orm(column_type = "String(StringLen::N(128))")]
    pub embedding_model: String,
    pub ingested_at: DateTimeUtc,
    pub updated_at: DateTimeUtc,
    #[sea_orm(has_many)]
    pub chunks: HasMany<super::document_chunk::Entity>,
}

impl ActiveModelBehavior for ActiveModel {}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use sea_orm::ActiveValue::Set;

    use super::*;
    use crate::entity::document_chunk::{self, EMBEDDING_DIMENSIONS};
    use crate::test_db;

    fn valid_row(source_id: &str) -> ActiveModel {
        let now = Utc::now();
        ActiveModel {
            source: Set("crawler".to_owned()),
            source_id: Set(source_id.to_owned()),
            title: Set("Title".to_owned()),
            content: Set("Body text.".to_owned()),
            content_hash: Set("0".repeat(64)),
            page_type: Set(PageType::Documentation),
            keywords: Set(vec!["docs".to_owned(), "guide".to_owned()]),
            summary: Set("A short summary.".to_owned()),
            embedding_model: Set("google/embeddinggemma-300m".to_owned()),
            ingested_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
    }

    fn chunk_row(document_id: i64, ordinal: i32) -> document_chunk::ActiveModel {
        document_chunk::ActiveModel {
            document_id: Set(document_id),
            ordinal: Set(ordinal),
            content: Set("A passage.".to_owned()),
            embedding: Set(PgVector::from(vec![0.5; EMBEDDING_DIMENSIONS as usize])),
            ..Default::default()
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_schema_holds_every_column_type_and_round_trips_a_row() {
        let test = test_db::start().await;

        let inserted = valid_row("https://example.com/a")
            .insert(&test.db)
            .await
            .unwrap();
        let chunk = chunk_row(inserted.id, 0).insert(&test.db).await.unwrap();

        assert_eq!(inserted.keywords, ["docs", "guide"]);
        assert_eq!(inserted.page_type, PageType::Documentation);
        assert_eq!(
            chunk.embedding.to_vec(),
            vec![0.5; EMBEDDING_DIMENSIONS as usize]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn source_and_source_id_together_must_be_unique() {
        let test = test_db::start().await;

        valid_row("https://example.com/a")
            .insert(&test.db)
            .await
            .unwrap();

        assert!(
            valid_row("https://example.com/a")
                .insert(&test.db)
                .await
                .is_err(),
            "a duplicate (source, source_id) was accepted"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_chunk_cannot_exist_without_its_document_and_goes_when_the_document_goes() {
        let test = test_db::start().await;
        let document = valid_row("https://example.com/a")
            .insert(&test.db)
            .await
            .unwrap();
        chunk_row(document.id, 0).insert(&test.db).await.unwrap();

        assert!(
            chunk_row(document.id + 1000, 0)
                .insert(&test.db)
                .await
                .is_err(),
            "a chunk pointing at no document was accepted"
        );
        assert!(
            chunk_row(document.id, 0).insert(&test.db).await.is_err(),
            "a duplicate (document_id, ordinal) was accepted"
        );

        Entity::delete_by_id(document.id)
            .exec(&test.db)
            .await
            .unwrap();
        assert!(
            document_chunk::Entity::find()
                .all(&test.db)
                .await
                .unwrap()
                .is_empty()
        );
    }
}
