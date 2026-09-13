// Checks each request at the boundary, then runs ingest or hybrid retrieval over passages.

use common::proto::knowledge_base::v1::{
    IngestRequest, IngestResponse, SearchRequest, SearchResponse, SearchResult,
};
use connectrpc::ConnectError;

use crate::entity::document::PageType;
use crate::ingest::{self, IngestFailed};
use crate::llm_client::{EmbedKind, LlmClient};
use crate::search;
use crate::store::{self, DocumentStore, Ranked};

const MAX_SOURCE_CHARS: usize = 64;
const MAX_SOURCE_ID_CHARS: usize = 2048;
const MAX_TITLE_CHARS: usize = 512;
const MAX_CONTENT_CHARS: usize = 200_000;
const MAX_QUERY_CHARS: usize = 1_000;
const MAX_PAGE_TYPES: usize = 6;
const DEFAULT_SEARCH_LIMIT: usize = 10;
const MAX_SEARCH_LIMIT: usize = 50;

pub struct KnowledgeBase<D: DocumentStore, L: LlmClient> {
    documents: D,
    llm: L,
}

impl<D: DocumentStore, L: LlmClient> KnowledgeBase<D, L> {
    pub fn new(documents: D, llm: L) -> Self {
        Self { documents, llm }
    }

    pub async fn ingest(&self, request: IngestRequest) -> Result<IngestResponse, ConnectError> {
        validate(&request)?;
        ingest::run(&self.documents, &self.llm, request)
            .await
            .map_err(ingest_error)
    }

    pub async fn search(&self, request: SearchRequest) -> Result<SearchResponse, ConnectError> {
        let page_types = parse_query(&request)?;
        let limit = match request.limit as usize {
            0 => DEFAULT_SEARCH_LIMIT,
            limit if limit > MAX_SEARCH_LIMIT => {
                return Err(ConnectError::invalid_argument(format!(
                    "limit is above {MAX_SEARCH_LIMIT}"
                )));
            }
            limit => limit,
        };
        let ranked = self.retrieve(&request.query, page_types).await?;

        Ok(SearchResponse {
            results: store::best_per_document(ranked, limit)
                .into_iter()
                .map(search_result)
                .collect(),
            ..Default::default()
        })
    }

    async fn retrieve(
        &self,
        raw_query: &str,
        page_types: Vec<PageType>,
    ) -> Result<Vec<Ranked>, ConnectError> {
        let query = search::normalize(raw_query);
        let lexical_query = search::any_word_lexical_query(&query);
        let (semantic, lexical) = tokio::join!(
            async {
                let embedded = self
                    .llm
                    .embed(&query, EmbedKind::SearchQuery)
                    .await
                    .map_err(|message| {
                        tracing::warn!("retrieval could not embed the query: {message}");
                        ConnectError::unavailable("could not embed the query")
                    })?;
                self.documents
                    .nearest(embedded.values, page_types.clone())
                    .await
                    .map_err(store_unavailable)
            },
            async {
                match &lexical_query {
                    Some(lexical_query) => self
                        .documents
                        .lexical(lexical_query, page_types.clone())
                        .await
                        .map_err(store_unavailable),
                    None => Ok(store::LexicalCandidates {
                        passages: Vec::new(),
                        titles: Vec::new(),
                    }),
                }
            }
        );
        let lexical = lexical?;
        Ok(store::fuse_with_document_titles(
            vec![semantic?, lexical.passages],
            lexical.titles,
        ))
    }
}

fn ingest_error(error: IngestFailed) -> ConnectError {
    match error {
        IngestFailed::Llm(message) => {
            tracing::warn!("ingest could not enrich or embed: {message}");
            ConnectError::unavailable("could not enrich or embed this content; nothing was written")
        }
        IngestFailed::Store(message) => {
            tracing::error!("ingest could not write the document: {message}");
            ConnectError::unavailable("the document store is not available")
        }
    }
}

fn search_result(ranked: Ranked) -> SearchResult {
    let passage = ranked.passage;
    SearchResult {
        source: passage.source,
        source_id: passage.source_id,
        title: passage.title,
        summary: passage.summary,
        page_type: search::page_type_name(passage.page_type).to_owned(),
        keywords: passage.keywords,
        snippet: passage.content,
        updated_at: passage.updated_at.to_rfc3339(),
        score: ranked.score as f32,
        ..Default::default()
    }
}

fn store_unavailable(error: sea_orm::DbErr) -> ConnectError {
    tracing::error!("retrieval could not query the document store: {error}");
    ConnectError::unavailable("the document store is not available")
}

fn parse_query(request: &SearchRequest) -> Result<Vec<PageType>, ConnectError> {
    if request.query.trim().is_empty() {
        return Err(ConnectError::invalid_argument("query is required"));
    }
    if request.query.chars().count() > MAX_QUERY_CHARS {
        return Err(ConnectError::invalid_argument(format!(
            "query is longer than {MAX_QUERY_CHARS} characters"
        )));
    }
    if request.page_types.len() > MAX_PAGE_TYPES {
        return Err(ConnectError::invalid_argument(format!(
            "at most {MAX_PAGE_TYPES} page types"
        )));
    }
    request
        .page_types
        .iter()
        .map(|name| {
            search::page_type_named(name).ok_or_else(|| {
                ConnectError::invalid_argument(format!("unknown page type {name:?}"))
            })
        })
        .collect()
}

fn validate(request: &IngestRequest) -> Result<(), ConnectError> {
    if request.source.trim().is_empty() {
        return Err(ConnectError::invalid_argument("source is required"));
    }
    if request.source_id.trim().is_empty() {
        return Err(ConnectError::invalid_argument("source_id is required"));
    }
    if request.content.trim().is_empty() {
        return Err(ConnectError::invalid_argument("content is required"));
    }
    if request.source.chars().count() > MAX_SOURCE_CHARS {
        return Err(ConnectError::invalid_argument(format!(
            "source is longer than {MAX_SOURCE_CHARS} characters"
        )));
    }
    if request.source_id.chars().count() > MAX_SOURCE_ID_CHARS {
        return Err(ConnectError::invalid_argument(format!(
            "source_id is longer than {MAX_SOURCE_ID_CHARS} characters"
        )));
    }
    if request.title.chars().count() > MAX_TITLE_CHARS {
        return Err(ConnectError::invalid_argument(format!(
            "title is longer than {MAX_TITLE_CHARS} characters"
        )));
    }
    if request.content.chars().count() > MAX_CONTENT_CHARS {
        return Err(ConnectError::invalid_argument(format!(
            "content is longer than {MAX_CONTENT_CHARS} characters"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
