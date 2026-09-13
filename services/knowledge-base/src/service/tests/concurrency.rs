// Proves all retrieval branches enter concurrently.

use std::sync::Arc;
use std::time::Duration;

use sea_orm::DbErr;

use super::super::*;
use super::support::{FakeLlm, passage, search_request};
use crate::store::{DocumentStore, DocumentWrite, LexicalCandidates, Passage};

#[derive(Clone)]
struct ConcurrentDocuments {
    retrievals: Arc<tokio::sync::Barrier>,
}

impl ConcurrentDocuments {
    fn new() -> Self {
        Self {
            retrievals: Arc::new(tokio::sync::Barrier::new(2)),
        }
    }

    async fn arrive(&self) -> Vec<Passage> {
        self.retrievals.wait().await;
        vec![passage(1, "a")]
    }
}

impl DocumentStore for ConcurrentDocuments {
    async fn existing_hash(&self, _: &str, _: &str) -> Result<Option<String>, DbErr> {
        Ok(None)
    }

    async fn upsert(&self, _: DocumentWrite) -> Result<(), DbErr> {
        Ok(())
    }

    async fn nearest(&self, _: Vec<f32>, _: Vec<PageType>) -> Result<Vec<Passage>, DbErr> {
        Ok(self.arrive().await)
    }

    async fn lexical(&self, _: &str, _: Vec<PageType>) -> Result<LexicalCandidates, DbErr> {
        Ok(LexicalCandidates {
            passages: self.arrive().await,
            titles: Vec::new(),
        })
    }
}

#[tokio::test]
async fn search_retrievers_stay_within_their_parallel_latency_budget() {
    const CONCURRENCY_BUDGET: Duration = Duration::from_millis(100);
    let knowledge_base = KnowledgeBase::new(ConcurrentDocuments::new(), FakeLlm::default());

    let response = tokio::time::timeout(
        CONCURRENCY_BUDGET,
        knowledge_base.search(search_request("wireless headphones", &[])),
    )
    .await
    .expect("retrievers did not all start concurrently")
    .unwrap();

    assert_eq!(response.results.len(), 1);
}
