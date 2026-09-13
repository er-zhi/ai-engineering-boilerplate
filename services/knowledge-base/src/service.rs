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

        match ingest::run(&self.documents, &self.llm, request).await {
            Ok(outcome) => Ok(outcome),
            Err(IngestFailed::Llm(message)) => {
                tracing::warn!("ingest could not enrich or embed: {message}");
                Err(ConnectError::unavailable(
                    "could not enrich or embed this content; nothing was written",
                ))
            }
            Err(IngestFailed::Store(message)) => {
                tracing::error!("ingest could not write the document: {message}");
                Err(ConnectError::unavailable(
                    "the document store is not available",
                ))
            }
        }
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
                match search::any_word_lexical_query(&query) {
                    Some(lexical_query) => self
                        .documents
                        .lexical(&lexical_query, page_types.clone())
                        .await
                        .map_err(store_unavailable),
                    None => Ok(Vec::new()),
                }
            }
        );
        Ok(store::fuse(vec![semantic?, lexical?]))
    }
}

fn search_result(ranked: Ranked) -> SearchResult {
    let (passage, document) = ranked.passage;
    SearchResult {
        source: document.source,
        source_id: document.source_id,
        title: document.title,
        summary: document.summary,
        page_type: search::page_type_name(document.page_type).to_owned(),
        keywords: document.keywords,
        snippet: passage.content,
        updated_at: document.updated_at.to_rfc3339(),
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
mod tests {
    use std::sync::{Arc, Mutex};

    use chrono::DateTime;
    use sea_orm::DbErr;
    use sea_orm::entity::prelude::PgVector;

    use super::*;
    use crate::entity::document_chunk::EMBEDDING_DIMENSIONS;
    use crate::entity::{document, document_chunk};
    use crate::llm_client::{Embedded, Enrichment};
    use crate::store::{DocumentWrite, Passage};

    type Calls<T> = Arc<Mutex<Vec<(T, Vec<PageType>)>>>;

    #[derive(Clone, Default)]
    struct MemoryDocuments {
        upserted: Arc<Mutex<Vec<DocumentWrite>>>,
        nearest: Vec<Passage>,
        lexical: Vec<Passage>,
        nearest_with: Calls<Vec<f32>>,
        lexical_with: Calls<String>,
        fails: bool,
    }

    impl DocumentStore for MemoryDocuments {
        async fn existing_hash(&self, _: &str, _: &str) -> Result<Option<String>, DbErr> {
            Ok(None)
        }

        async fn upsert(&self, document: DocumentWrite) -> Result<(), DbErr> {
            self.upserted.lock().unwrap().push(document);
            Ok(())
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
        ) -> Result<Vec<Passage>, DbErr> {
            self.lexical_with
                .lock()
                .unwrap()
                .push((lexical_query.to_owned(), page_types));
            if self.fails {
                return Err(DbErr::Custom("database is down".into()));
            }
            Ok(self.lexical.clone())
        }
    }

    #[derive(Clone, Default)]
    struct FakeLlm {
        embed_fails: bool,
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
                model_used: "Qwen/Qwen3-Embedding-0.6B".to_owned(),
            })
        }
    }

    fn passage(chunk_id: i64, source_id: &str) -> Passage {
        let document_id = source_id.bytes().map(i64::from).sum();
        (
            document_chunk::Model {
                id: chunk_id,
                document_id,
                ordinal: 0,
                content: format!("passage {chunk_id}"),
                embedding: PgVector::from(vec![0.0; EMBEDDING_DIMENSIONS as usize]),
            },
            document::Model {
                id: document_id,
                source: "crawler".to_owned(),
                source_id: source_id.to_owned(),
                title: format!("Title {source_id}"),
                content: String::new(),
                content_hash: "0".repeat(64),
                page_type: PageType::Product,
                keywords: vec!["wireless".to_owned(), "headphones".to_owned()],
                summary: "Summary.".to_owned(),
                embedding_model: "model".to_owned(),
                ingested_at: DateTime::UNIX_EPOCH,
                updated_at: DateTime::from_timestamp(1_789_000_000, 0).unwrap(),
            },
        )
    }

    fn request(source: &str, source_id: &str, content: &str) -> IngestRequest {
        IngestRequest {
            source: source.to_owned(),
            source_id: source_id.to_owned(),
            title: "Title".to_owned(),
            content: content.to_owned(),
            ..Default::default()
        }
    }

    fn search_request(query: &str, page_types: &[&str]) -> SearchRequest {
        SearchRequest {
            query: query.to_owned(),
            page_types: page_types.iter().map(|name| (*name).to_owned()).collect(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn a_valid_request_is_stored() {
        let documents = MemoryDocuments::default();
        let knowledge_base = KnowledgeBase::new(documents.clone(), FakeLlm::default());

        let response = knowledge_base
            .ingest(request("crawler", "https://example.com/a", "hello world"))
            .await
            .unwrap();

        assert!(response.stored);
        assert_eq!(documents.upserted.lock().unwrap()[0].chunks.len(), 1);
    }

    #[tokio::test]
    async fn an_empty_source_is_refused_before_any_model_is_called() {
        let documents = MemoryDocuments::default();
        let knowledge_base = KnowledgeBase::new(documents.clone(), FakeLlm::default());

        let error = knowledge_base
            .ingest(request("", "https://example.com/a", "hello world"))
            .await
            .unwrap_err();

        assert!(format!("{error:?}").contains("source"), "{error:?}");
        assert!(documents.upserted.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_empty_source_id_is_refused() {
        let knowledge_base = KnowledgeBase::new(MemoryDocuments::default(), FakeLlm::default());

        let error = knowledge_base
            .ingest(request("crawler", "", "hello world"))
            .await
            .unwrap_err();

        assert!(format!("{error:?}").contains("source_id"), "{error:?}");
    }

    #[tokio::test]
    async fn empty_content_is_refused() {
        let knowledge_base = KnowledgeBase::new(MemoryDocuments::default(), FakeLlm::default());

        let error = knowledge_base
            .ingest(request("crawler", "https://example.com/a", "  "))
            .await
            .unwrap_err();

        assert!(format!("{error:?}").contains("content"), "{error:?}");
    }

    #[tokio::test]
    async fn an_oversized_source_id_is_refused() {
        let knowledge_base = KnowledgeBase::new(MemoryDocuments::default(), FakeLlm::default());

        let error = knowledge_base
            .ingest(request(
                "crawler",
                &"u".repeat(MAX_SOURCE_ID_CHARS + 1),
                "hello world",
            ))
            .await
            .unwrap_err();

        assert!(format!("{error:?}").contains("source_id"), "{error:?}");
    }

    #[tokio::test]
    async fn multibyte_source_and_source_id_are_limited_by_characters() {
        let documents = MemoryDocuments::default();
        let knowledge_base = KnowledgeBase::new(documents.clone(), FakeLlm::default());

        let response = knowledge_base
            .ingest(request(
                &"é".repeat(MAX_SOURCE_CHARS),
                &"界".repeat(MAX_SOURCE_ID_CHARS),
                "hello world",
            ))
            .await
            .unwrap();

        assert!(response.stored);
        assert_eq!(documents.upserted.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_search_fuses_passages_and_returns_each_document_once_with_its_best_passage() {
        let documents = MemoryDocuments {
            nearest: vec![passage(1, "a"), passage(2, "b"), passage(3, "a")],
            lexical: vec![passage(2, "b"), passage(3, "a")],
            ..MemoryDocuments::default()
        };
        let knowledge_base = KnowledgeBase::new(documents.clone(), FakeLlm::default());

        let response = knowledge_base
            .search(search_request(
                "  Wireless   PRODUCT? ",
                &["product", "blog"],
            ))
            .await
            .unwrap();

        let results: Vec<(&str, &str)> = response
            .results
            .iter()
            .map(|result| (result.source_id.as_str(), result.snippet.as_str()))
            .collect();
        assert_eq!(results, [("b", "passage 2"), ("a", "passage 3")]);
        assert_eq!(response.results[0].page_type, "product");
        assert_eq!(response.results[0].keywords, ["wireless", "headphones"]);
        assert_eq!(response.results[0].updated_at, "2026-09-10T00:26:40+00:00");
        assert!(response.results[0].score > response.results[1].score);

        let wanted = vec![PageType::Product, PageType::Blog];
        assert_eq!(
            documents.nearest_with.lock().unwrap()[0],
            (vec![0.1, 0.2, 0.3], wanted.clone())
        );
        assert_eq!(
            documents.lexical_with.lock().unwrap()[0],
            ("wireless | product".to_owned(), wanted)
        );
    }

    fn many_documents(count: i64) -> MemoryDocuments {
        MemoryDocuments {
            nearest: (1..=count)
                .map(|n| passage(n, &format!("doc-{n}")))
                .collect(),
            ..MemoryDocuments::default()
        }
    }

    #[tokio::test]
    async fn search_returns_ten_documents_by_default_and_honours_a_limit() {
        let knowledge_base = KnowledgeBase::new(many_documents(30), FakeLlm::default());

        let default = knowledge_base
            .search(search_request("headphones", &[]))
            .await
            .unwrap();
        let limited = knowledge_base
            .search(SearchRequest {
                limit: 3,
                ..search_request("headphones", &[])
            })
            .await
            .unwrap();

        assert_eq!(default.results.len(), DEFAULT_SEARCH_LIMIT);
        assert_eq!(limited.results.len(), 3);
    }

    #[tokio::test]
    async fn a_limit_above_the_maximum_is_refused() {
        let knowledge_base = KnowledgeBase::new(many_documents(1), FakeLlm::default());

        let error = knowledge_base
            .search(SearchRequest {
                limit: MAX_SEARCH_LIMIT as u32 + 1,
                ..search_request("headphones", &[])
            })
            .await
            .unwrap_err();

        assert!(format!("{error:?}").contains("limit"), "{error:?}");
    }

    #[tokio::test]
    async fn too_many_page_types_are_refused() {
        let knowledge_base = KnowledgeBase::new(MemoryDocuments::default(), FakeLlm::default());
        let names = ["product"; MAX_PAGE_TYPES + 1];

        let error = knowledge_base
            .search(search_request("headphones", &names))
            .await
            .unwrap_err();

        assert!(format!("{error:?}").contains("page types"), "{error:?}");
    }

    #[tokio::test]
    async fn an_unknown_page_type_is_refused_before_anything_runs() {
        let documents = MemoryDocuments::default();
        let knowledge_base = KnowledgeBase::new(documents.clone(), FakeLlm::default());

        let error = knowledge_base
            .search(search_request("headphones", &["faq"]))
            .await
            .unwrap_err();

        assert!(format!("{error:?}").contains("faq"), "{error:?}");
        assert!(documents.nearest_with.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_empty_or_overlong_query_is_refused_before_anything_runs() {
        let documents = MemoryDocuments::default();
        let knowledge_base = KnowledgeBase::new(documents.clone(), FakeLlm::default());

        for query in [String::new(), "q".repeat(MAX_QUERY_CHARS + 1)] {
            let error = knowledge_base
                .search(search_request(&query, &[]))
                .await
                .unwrap_err();
            assert!(format!("{error:?}").contains("query"), "{error:?}");
        }
        assert!(documents.nearest_with.lock().unwrap().is_empty());
        assert!(documents.lexical_with.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_query_with_no_words_skips_the_lexical_retriever() {
        let documents = MemoryDocuments {
            nearest: vec![passage(1, "a")],
            ..MemoryDocuments::default()
        };
        let knowledge_base = KnowledgeBase::new(documents.clone(), FakeLlm::default());

        let response = knowledge_base
            .search(search_request("?!", &[]))
            .await
            .unwrap();

        assert_eq!(response.results.len(), 1);
        assert!(documents.lexical_with.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_embed_failure_fails_the_search_but_the_lexical_side_still_ran() {
        let documents = MemoryDocuments::default();
        let llm = FakeLlm { embed_fails: true };
        let knowledge_base = KnowledgeBase::new(documents.clone(), llm);

        let error = knowledge_base
            .search(search_request("wireless", &[]))
            .await
            .unwrap_err();

        assert!(format!("{error:?}").contains("embed"), "{error:?}");
        assert_eq!(documents.lexical_with.lock().unwrap().len(), 1);
        assert!(documents.nearest_with.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_store_failure_is_reported_as_unavailable() {
        let documents = MemoryDocuments {
            fails: true,
            ..MemoryDocuments::default()
        };
        let knowledge_base = KnowledgeBase::new(documents, FakeLlm::default());

        let error = knowledge_base
            .search(search_request("wireless", &[]))
            .await
            .unwrap_err();

        assert!(format!("{error:?}").contains("document store"), "{error:?}");
    }
}
