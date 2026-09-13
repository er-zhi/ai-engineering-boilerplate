// The document store: dedup lookup, transactional writes, and indexable retrieval candidates.

use chrono::Utc;
use sea_orm::entity::prelude::DateTimeUtc;
use sea_orm::sea_query::OnConflict;
use sea_orm::{
    ActiveValue::Set, ColumnTrait, DatabaseConnection, DbErr, EntityTrait, FromQueryResult,
    QueryFilter, TransactionTrait,
};

use crate::entity::document::{self, PageType};
use crate::entity::document_chunk;

mod ranking;
mod retrieval;

pub use ranking::{best_per_document, fuse_with_document_titles};

#[cfg(test)]
pub use ranking::fuse;

#[cfg(test)]
use retrieval::{lexical_passage_candidates, lexical_title_candidates, passage_projection};

pub struct DocumentWrite {
    pub document: document::ActiveModel,
    pub chunks: Vec<document_chunk::ActiveModel>,
}

#[derive(Clone, Debug, PartialEq, FromQueryResult)]
pub struct Passage {
    pub chunk_id: i64,
    pub document_id: i64,
    pub content: String,
    pub source: String,
    pub source_id: String,
    pub title: String,
    pub summary: String,
    pub page_type: PageType,
    pub keywords: Vec<String>,
    pub updated_at: DateTimeUtc,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Ranked {
    pub passage: Passage,
    pub score: f64,
}

pub struct LexicalCandidates {
    pub passages: Vec<Passage>,
    pub titles: Vec<Passage>,
}

pub trait DocumentStore: Clone + Send + Sync + 'static {
    fn existing_hash(
        &self,
        source: &str,
        source_id: &str,
    ) -> impl Future<Output = Result<Option<String>, DbErr>> + Send;

    fn upsert(&self, document: DocumentWrite) -> impl Future<Output = Result<(), DbErr>> + Send;

    fn nearest(
        &self,
        query_embedding: Vec<f32>,
        page_types: Vec<PageType>,
    ) -> impl Future<Output = Result<Vec<Passage>, DbErr>> + Send;

    fn lexical(
        &self,
        lexical_query: &str,
        page_types: Vec<PageType>,
    ) -> impl Future<Output = Result<LexicalCandidates, DbErr>> + Send;
}

#[derive(Clone)]
pub struct PgDocuments {
    db: DatabaseConnection,
}

impl PgDocuments {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }
}

impl DocumentStore for PgDocuments {
    async fn existing_hash(&self, source: &str, source_id: &str) -> Result<Option<String>, DbErr> {
        document::Entity::find()
            .filter(document::Column::Source.eq(source))
            .filter(document::Column::SourceId.eq(source_id))
            .one(&self.db)
            .await
            .map(|row| row.map(|found| found.content_hash))
    }

    async fn upsert(&self, mut write: DocumentWrite) -> Result<(), DbErr> {
        let now = Utc::now();
        write.document.ingested_at = Set(now);
        write.document.updated_at = Set(now);

        let txn = self.db.begin().await?;
        let document_id = document::Entity::insert(write.document)
            .on_conflict(
                OnConflict::columns([document::Column::Source, document::Column::SourceId])
                    .update_columns([
                        document::Column::Title,
                        document::Column::Content,
                        document::Column::ContentHash,
                        document::Column::PageType,
                        document::Column::Keywords,
                        document::Column::Summary,
                        document::Column::EmbeddingModel,
                        document::Column::UpdatedAt,
                    ])
                    .to_owned(),
            )
            .exec(&txn)
            .await?
            .last_insert_id;

        document_chunk::Entity::delete_many()
            .filter(document_chunk::Column::DocumentId.eq(document_id))
            .exec(&txn)
            .await?;
        for (ordinal, chunk) in write.chunks.iter_mut().enumerate() {
            chunk.document_id = Set(document_id);
            chunk.ordinal = Set(i32::try_from(ordinal)
                .map_err(|_| DbErr::Custom(format!("passage ordinal {ordinal} overflows")))?);
        }
        if !write.chunks.is_empty() {
            document_chunk::Entity::insert_many(write.chunks)
                .exec(&txn)
                .await?;
        }
        txn.commit().await
    }

    async fn nearest(
        &self,
        query_embedding: Vec<f32>,
        page_types: Vec<PageType>,
    ) -> Result<Vec<Passage>, DbErr> {
        retrieval::nearest(&self.db, query_embedding, page_types).await
    }

    async fn lexical(
        &self,
        lexical_query: &str,
        page_types: Vec<PageType>,
    ) -> Result<LexicalCandidates, DbErr> {
        let passages =
            retrieval::lexical_passages(&self.db, lexical_query, page_types.clone()).await?;
        let titles = retrieval::lexical_titles(&self.db, lexical_query, page_types).await?;
        Ok(LexicalCandidates { passages, titles })
    }
}

#[cfg(test)]
mod tests;
