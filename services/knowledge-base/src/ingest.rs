// Hashes incoming content, skips LLM and embedding spend when it matches what is already stored, and otherwise enriches it, splits it into passages, embeds each with a title-and-summary header, and writes it all at once.

use sea_orm::ActiveValue::Set;
use sea_orm::entity::prelude::PgVector;
use sha2::{Digest, Sha256};

use common::proto::knowledge_base::v1::{IngestRequest, IngestResponse};

use crate::chunk;
use crate::entity::document::MAX_EMBEDDING_MODEL_CHARS;
use crate::entity::{document, document_chunk};
use crate::llm_client::{EmbedKind, LlmClient};
use crate::store::{DocumentStore, DocumentWrite};

const MAX_HEADER_TITLE_CHARS: usize = 150;
const MAX_HEADER_SUMMARY_CHARS: usize = 300;

#[derive(Debug, PartialEq, Eq)]
pub enum IngestFailed {
    Llm(String),
    Store(String),
}

pub async fn run(
    documents: &impl DocumentStore,
    llm: &impl LlmClient,
    input: IngestRequest,
) -> Result<IngestResponse, IngestFailed> {
    let content_hash = sha256_hex(&input.content);

    let existing = documents
        .existing_hash(&input.source, &input.source_id)
        .await
        .map_err(|error| IngestFailed::Store(error.to_string()))?;
    if existing.as_deref() == Some(content_hash.as_str()) {
        return Ok(IngestResponse {
            stored: false,
            skipped: true,
            ..Default::default()
        });
    }

    let enrichment = llm
        .enrich(&input.content)
        .await
        .map_err(IngestFailed::Llm)?;

    let header = context_header(&input.title, &enrichment.summary);
    let (chunks, embedding_model) = embed_chunks(llm, &input.content, &header).await?;

    documents
        .upsert(DocumentWrite {
            document: document::ActiveModel {
                source: Set(input.source),
                source_id: Set(input.source_id),
                title: Set(input.title),
                content: Set(input.content),
                content_hash: Set(content_hash),
                page_type: Set(enrichment.page_type),
                keywords: Set(enrichment.keywords),
                summary: Set(enrichment.summary),
                embedding_model: Set(embedding_model),
                ..Default::default()
            },
            chunks,
        })
        .await
        .map_err(|error| IngestFailed::Store(error.to_string()))?;

    Ok(IngestResponse {
        stored: true,
        skipped: false,
        ..Default::default()
    })
}

async fn embed_chunks(
    llm: &impl LlmClient,
    content: &str,
    header: &str,
) -> Result<(Vec<document_chunk::ActiveModel>, String), IngestFailed> {
    let mut chunks = Vec::new();
    let mut embedding_model = String::new();
    for passage in chunk::split(content) {
        let embedded = llm
            .embed(&format!("{header}{passage}"), EmbedKind::StoredPassage)
            .await
            .map_err(IngestFailed::Llm)?;
        embedding_model = embedded
            .model_used
            .chars()
            .take(MAX_EMBEDDING_MODEL_CHARS)
            .collect();
        chunks.push(document_chunk::ActiveModel {
            content: Set(passage),
            embedding: Set(PgVector::from(embedded.values)),
            ..Default::default()
        });
    }
    Ok((chunks, embedding_model))
}

fn context_header(title: &str, summary: &str) -> String {
    let title: String = title.chars().take(MAX_HEADER_TITLE_CHARS).collect();
    let summary: String = summary.chars().take(MAX_HEADER_SUMMARY_CHARS).collect();
    format!("{title}\n{summary}\n\n")
}

fn sha256_hex(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use sea_orm::DbErr;

    use super::*;
    use crate::entity::document::PageType;
    use crate::llm_client::{Embedded, Enrichment};
    use crate::store::Passage;

    #[derive(Clone, Default)]
    struct FakeDocuments {
        hashes: Arc<Mutex<Vec<(String, String, String)>>>,
        upserted: Arc<Mutex<Vec<DocumentWrite>>>,
        upsert_fails: bool,
    }

    impl FakeDocuments {
        fn with_existing(source: &str, source_id: &str, hash: &str) -> Self {
            let store = Self::default();
            store.hashes.lock().unwrap().push((
                source.to_owned(),
                source_id.to_owned(),
                hash.to_owned(),
            ));
            store
        }

        fn failing_upsert() -> Self {
            Self {
                upsert_fails: true,
                ..Self::default()
            }
        }
    }

    impl DocumentStore for FakeDocuments {
        async fn existing_hash(
            &self,
            source: &str,
            source_id: &str,
        ) -> Result<Option<String>, DbErr> {
            Ok(self
                .hashes
                .lock()
                .unwrap()
                .iter()
                .find(|(s, id, _)| s == source && id == source_id)
                .map(|(_, _, hash)| hash.clone()))
        }

        async fn upsert(&self, document: DocumentWrite) -> Result<(), DbErr> {
            if self.upsert_fails {
                return Err(DbErr::Custom("database is down".into()));
            }
            self.upserted.lock().unwrap().push(document);
            Ok(())
        }

        async fn nearest(&self, _: Vec<f32>, _: Vec<PageType>) -> Result<Vec<Passage>, DbErr> {
            Err(DbErr::Custom("ingest tests do not exercise search".into()))
        }

        async fn lexical(
            &self,
            _: &str,
            _: Vec<PageType>,
        ) -> Result<crate::store::LexicalCandidates, DbErr> {
            Err(DbErr::Custom("ingest tests do not exercise search".into()))
        }
    }

    #[derive(Clone, Default)]
    struct FakeLlm {
        enrich_fails: bool,
        embed_fails: bool,
        embedded_texts: Arc<Mutex<Vec<String>>>,
        model_used: Option<String>,
    }

    impl LlmClient for FakeLlm {
        async fn enrich(&self, _content: &str) -> Result<Enrichment, String> {
            if self.enrich_fails {
                return Err("the model refused".to_owned());
            }
            Ok(Enrichment {
                page_type: PageType::Documentation,
                keywords: vec!["docs".to_owned()],
                summary: "A summary.".to_owned(),
            })
        }

        async fn embed(&self, content: &str, kind: EmbedKind) -> Result<Embedded, String> {
            assert_eq!(kind, EmbedKind::StoredPassage);
            if self.embed_fails {
                return Err("the embedder is unavailable".to_owned());
            }
            self.embedded_texts.lock().unwrap().push(content.to_owned());
            Ok(Embedded {
                values: vec![0.1, 0.2, 0.3],
                model_used: self
                    .model_used
                    .clone()
                    .unwrap_or_else(|| "Qwen/Qwen3-Embedding-0.6B".to_owned()),
            })
        }
    }

    fn content(text: &str) -> IngestRequest {
        IngestRequest {
            source: "crawler".to_owned(),
            source_id: "https://example.com/a".to_owned(),
            title: "Title".to_owned(),
            content: text.to_owned(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn new_content_is_enriched_split_embedded_with_its_header_and_stored() {
        let documents = FakeDocuments::default();
        let llm = FakeLlm::default();
        let long = "A sentence about managed agents and event streams. ".repeat(60);

        let result = run(&documents, &llm, content(&long)).await.unwrap();

        assert_eq!(
            result,
            IngestResponse {
                stored: true,
                skipped: false,
                ..Default::default()
            }
        );
        let upserted = documents.upserted.lock().unwrap();
        let document = &upserted[0];
        assert_eq!(
            document.document.page_type.clone().unwrap(),
            PageType::Documentation
        );
        assert_eq!(document.document.content.clone().unwrap(), long);
        assert_eq!(
            document.document.embedding_model.clone().unwrap(),
            "Qwen/Qwen3-Embedding-0.6B"
        );
        assert!(document.chunks.len() > 1);
        let embedded = llm.embedded_texts.lock().unwrap();
        assert_eq!(embedded.len(), document.chunks.len());
        for (text, chunk) in embedded.iter().zip(&document.chunks) {
            let chunk_content = chunk.content.clone().unwrap();
            assert_eq!(text, &format!("Title\nA summary.\n\n{chunk_content}"));
            assert!(!chunk_content.starts_with("Title"));
        }
    }

    #[tokio::test]
    async fn an_overlong_embedding_model_name_is_truncated_before_storage() {
        let documents = FakeDocuments::default();
        let llm = FakeLlm {
            model_used: Some("m".repeat(MAX_EMBEDDING_MODEL_CHARS + 1)),
            ..FakeLlm::default()
        };

        run(&documents, &llm, content("hello")).await.unwrap();

        let stored = documents.upserted.lock().unwrap();
        assert_eq!(
            stored[0]
                .document
                .embedding_model
                .clone()
                .unwrap()
                .chars()
                .count(),
            MAX_EMBEDDING_MODEL_CHARS
        );
    }

    #[test]
    fn the_header_keeps_only_the_start_of_a_long_title_and_summary() {
        let header = context_header(
            &"t".repeat(MAX_HEADER_TITLE_CHARS + 50),
            &"s".repeat(MAX_HEADER_SUMMARY_CHARS + 50),
        );

        assert_eq!(
            header,
            format!(
                "{}\n{}\n\n",
                "t".repeat(MAX_HEADER_TITLE_CHARS),
                "s".repeat(MAX_HEADER_SUMMARY_CHARS)
            )
        );
    }

    #[tokio::test]
    async fn matching_hash_skips_the_llm_and_the_store() {
        let documents =
            FakeDocuments::with_existing("crawler", "https://example.com/a", &sha256_hex("hello"));
        let llm = FakeLlm::default();

        let result = run(&documents, &llm, content("hello")).await.unwrap();

        assert!(result.skipped);
        assert!(documents.upserted.lock().unwrap().is_empty());
        assert!(llm.embedded_texts.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_changed_hash_is_enriched_and_stored_again() {
        let documents =
            FakeDocuments::with_existing("crawler", "https://example.com/a", &sha256_hex("old"));

        let result = run(&documents, &FakeLlm::default(), content("new content"))
            .await
            .unwrap();

        assert!(result.stored);
        assert_eq!(documents.upserted.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn an_enrich_failure_leaves_nothing_embedded_or_written() {
        let documents = FakeDocuments::default();
        let llm = FakeLlm {
            enrich_fails: true,
            ..FakeLlm::default()
        };

        let error = run(&documents, &llm, content("hello")).await.unwrap_err();

        assert_eq!(error, IngestFailed::Llm("the model refused".to_owned()));
        assert!(llm.embedded_texts.lock().unwrap().is_empty());
        assert!(documents.upserted.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_embed_failure_leaves_no_document_written() {
        let documents = FakeDocuments::default();
        let llm = FakeLlm {
            embed_fails: true,
            ..FakeLlm::default()
        };

        let error = run(&documents, &llm, content("hello")).await.unwrap_err();

        assert_eq!(
            error,
            IngestFailed::Llm("the embedder is unavailable".to_owned())
        );
        assert!(documents.upserted.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_store_failure_is_reported() {
        let error = run(
            &FakeDocuments::failing_upsert(),
            &FakeLlm::default(),
            content("hello"),
        )
        .await
        .unwrap_err();

        assert!(matches!(error, IngestFailed::Store(_)));
    }
}
