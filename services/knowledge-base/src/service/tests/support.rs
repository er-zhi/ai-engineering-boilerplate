// Provides in-memory service doubles and request fixtures.

use std::sync::{Arc, Mutex};

use chrono::DateTime;
use sea_orm::DbErr;

use super::super::*;
use common::proto::knowledge_base::v1::PageType as PageTypeProto;

use crate::entity::document;
use crate::llm_client::{Embedded, Enrichment};
use crate::store::{DocumentWrite, LexicalCandidates, Passage};

pub(super) type Calls<T> = Arc<Mutex<Vec<(T, Vec<PageType>)>>>;

#[derive(Clone, Default)]
pub(super) struct MemoryDocuments {
    pub(super) upserted: Arc<Mutex<Vec<DocumentWrite>>>,
    pub(super) nearest: Vec<Passage>,
    pub(super) lexical_passages: Vec<Passage>,
    pub(super) lexical_titles: Vec<Passage>,
    pub(super) nearest_with: Calls<Vec<f32>>,
    pub(super) lexical_passages_with: Calls<String>,
    pub(super) lexical_titles_with: Calls<String>,
    pub(super) fails: bool,
    pub(super) document: Option<document::Model>,
}

impl DocumentStore for MemoryDocuments {
    async fn existing_hash(&self, _: &str, _: &str) -> Result<Option<String>, DbErr> {
        Ok(None)
    }

    async fn upsert(&self, document: DocumentWrite) -> Result<(), DbErr> {
        self.upserted.lock().unwrap().push(document);
        Ok(())
    }

    async fn document(&self, _: &str, _: &str) -> Result<Option<document::Model>, DbErr> {
        if self.fails {
            return Err(DbErr::Custom("database is down".into()));
        }
        Ok(self.document.clone())
    }

    async fn nearest(
        &self,
        query_embedding: Vec<f32>,
        page_types: Vec<PageType>,
    ) -> Result<Vec<Passage>, DbErr> {
        self.nearest_with
            .lock()
            .unwrap()
            .push((query_embedding, page_types));
        if self.fails {
            return Err(DbErr::Custom("database is down".into()));
        }
        Ok(self.nearest.clone())
    }

    async fn lexical(
        &self,
        lexical_query: &str,
        page_types: Vec<PageType>,
    ) -> Result<LexicalCandidates, DbErr> {
        self.lexical_passages_with
            .lock()
            .unwrap()
            .push((lexical_query.to_owned(), page_types.clone()));
        self.lexical_titles_with
            .lock()
            .unwrap()
            .push((lexical_query.to_owned(), page_types));
        if self.fails {
            return Err(DbErr::Custom("database is down".into()));
        }
        Ok(LexicalCandidates {
            passages: self.lexical_passages.clone(),
            titles: self.lexical_titles.clone(),
        })
    }
}

#[derive(Clone, Default)]
pub(super) struct FakeLlm {
    pub(super) embed_fails: bool,
}

impl LlmClient for FakeLlm {
    async fn enrich(&self, _: &str) -> Result<Enrichment, String> {
        Ok(Enrichment {
            page_type: PageType::Documentation,
            keywords: vec!["docs".to_owned()],
            summary: "A summary.".to_owned(),
        })
    }

    async fn embed(&self, _: &str, _: EmbedKind) -> Result<Embedded, String> {
        if self.embed_fails {
            return Err("embedder is down".to_owned());
        }
        Ok(Embedded {
            values: vec![0.1, 0.2, 0.3],
            model_used: "google/embeddinggemma-300m".to_owned(),
        })
    }
}

pub(super) fn passage(chunk_id: i64, source_id: &str) -> Passage {
    let document_id = source_id.bytes().map(i64::from).sum();
    Passage {
        chunk_id,
        document_id,
        ordinal: 0,
        content: format!("passage {chunk_id}"),
        source: "crawler".to_owned(),
        source_id: source_id.to_owned(),
        title: format!("Title {source_id}"),
        summary: "Summary.".to_owned(),
        page_type: PageType::Product,
        keywords: vec!["wireless".to_owned(), "headphones".to_owned()],
        updated_at: DateTime::from_timestamp(1_789_000_000, 0).unwrap(),
        content_hash: "0".repeat(64),
    }
}

pub(super) fn request(source: &str, source_id: &str, content: &str) -> IngestRequest {
    IngestRequest {
        source: source.to_owned(),
        source_id: source_id.to_owned(),
        title: "Title".to_owned(),
        content: content.to_owned(),
        ..Default::default()
    }
}

pub(super) fn search_request(query: &str, page_types: &[PageTypeProto]) -> SearchRequest {
    SearchRequest {
        query: query.to_owned(),
        page_types: page_types.iter().copied().map(EnumValue::Known).collect(),
        ..Default::default()
    }
}
