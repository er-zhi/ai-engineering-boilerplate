// knowledge_base.document_chunks: a document's content split into retrieval-sized passages, each with its own embedding.

use sea_orm::entity::prelude::*;

pub const EMBEDDING_DIMENSIONS: u32 = 768;

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "document_chunks", schema_name = "knowledge_base")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(unique_key = "document_and_ordinal")]
    pub document_id: i64,
    #[sea_orm(unique_key = "document_and_ordinal")]
    pub ordinal: i32,
    #[sea_orm(column_type = "Text")]
    pub content: String,
    #[sea_orm(column_type = "Vector(Some(EMBEDDING_DIMENSIONS))")]
    pub embedding: PgVector,
    #[sea_orm(belongs_to, from = "document_id", to = "id", on_delete = "Cascade")]
    pub document: BelongsTo<super::document::Entity>,
}

impl ActiveModelBehavior for ActiveModel {}

pub const INDEX_STATEMENTS_CREATED_AFTER_SCHEMA_SYNC: [&str; 3] = [
    "CREATE INDEX IF NOT EXISTS document_chunks_embedding_hnsw \
     ON knowledge_base.document_chunks USING hnsw (embedding vector_cosine_ops)",
    "CREATE INDEX IF NOT EXISTS document_chunks_fulltext_gin \
     ON knowledge_base.document_chunks USING gin (to_tsvector('english', content))",
    "CREATE INDEX IF NOT EXISTS documents_title_fulltext_gin \
     ON knowledge_base.documents USING gin (to_tsvector('english', title))",
];
