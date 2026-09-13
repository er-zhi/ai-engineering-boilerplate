// What knowledge-base needs from llm-router (enrichment) and the native embedder (a vector).

use crate::entity::document::PageType;

#[derive(Clone, Debug, PartialEq)]
pub struct Enrichment {
    pub page_type: PageType,
    pub keywords: Vec<String>,
    pub summary: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Embedded {
    pub values: Vec<f32>,
    pub model_used: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmbedKind {
    StoredPassage,
    SearchQuery,
}

pub trait LlmClient: Send + Sync {
    fn enrich(&self, content: &str) -> impl Future<Output = Result<Enrichment, String>> + Send;
    fn embed(
        &self,
        content: &str,
        kind: EmbedKind,
    ) -> impl Future<Output = Result<Embedded, String>> + Send;
}
