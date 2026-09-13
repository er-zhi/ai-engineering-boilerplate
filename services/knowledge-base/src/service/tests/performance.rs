// Measures a warmed service search against real PostgreSQL.

use std::time::Duration;

use sea_orm::ActiveValue::Set;
use sea_orm::entity::prelude::PgVector;

use super::super::*;
use super::support::search_request;
use crate::entity::document_chunk::EMBEDDING_DIMENSIONS;
use crate::entity::{document, document_chunk};
use crate::llm_client::{Embedded, Enrichment};
use crate::store::{DocumentStore, DocumentWrite, PgDocuments};
use crate::test_db;

#[derive(Clone)]
struct DatabaseLlm;

impl LlmClient for DatabaseLlm {
    async fn enrich(&self, _: &str) -> Result<Enrichment, String> {
        Err("performance search does not enrich".to_owned())
    }

    async fn embed(&self, _: &str, _: EmbedKind) -> Result<Embedded, String> {
        let mut values = vec![0.0; EMBEDDING_DIMENSIONS as usize];
        values[0] = 1.0;
        Ok(Embedded {
            values,
            model_used: "test-embedding".to_owned(),
        })
    }
}

fn performance_document() -> DocumentWrite {
    let mut values = vec![0.0; EMBEDDING_DIMENSIONS as usize];
    values[0] = 1.0;
    let chunks = (0..64)
        .map(|ordinal| document_chunk::ActiveModel {
            content: Set(format!(
                "agent orchestration performance passage number {ordinal}"
            )),
            embedding: Set(PgVector::from(values.clone())),
            ..Default::default()
        })
        .collect();
    DocumentWrite {
        document: document::ActiveModel {
            source: Set("performance-test".to_owned()),
            source_id: Set("performance-document".to_owned()),
            title: Set("Agent orchestration performance".to_owned()),
            content: Set("agent orchestration performance".to_owned()),
            content_hash: Set("0".repeat(64)),
            page_type: Set(PageType::Documentation),
            keywords: Set(vec!["agent".to_owned()]),
            summary: Set("Performance fixture.".to_owned()),
            embedding_model: Set("test-embedding".to_owned()),
            ..Default::default()
        },
        chunks,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn end_to_end_search_with_postgres_stays_within_its_warm_latency_budget() {
    const WARM_SEARCH_BUDGET: Duration = Duration::from_secs(1);

    let test = test_db::start().await;
    let documents = PgDocuments::new(test.db.clone());
    documents.upsert(performance_document()).await.unwrap();
    let knowledge_base = KnowledgeBase::new(documents, DatabaseLlm);
    let request = search_request("agent orchestration performance", &[]);
    knowledge_base.search(request.clone()).await.unwrap();
    let started = tokio::time::Instant::now();

    let response = tokio::time::timeout(WARM_SEARCH_BUDGET, knowledge_base.search(request))
        .await
        .expect("warm search exceeded its latency budget")
        .unwrap();
    let elapsed = started.elapsed();

    assert_eq!(response.results.len(), 1);
    assert!(
        elapsed <= WARM_SEARCH_BUDGET,
        "warm search took {elapsed:?}, expected no more than {WARM_SEARCH_BUDGET:?}"
    );
}
