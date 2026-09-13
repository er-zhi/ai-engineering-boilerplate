// The document store: dedup lookup, writing a document with its passages in one transaction, and the two passage retrievers retrieval fuses.

use std::collections::{HashMap, HashSet};

use chrono::Utc;
use sea_orm::entity::prelude::PgVector;
use sea_orm::sea_query::{Expr, OnConflict, Order};
use sea_orm::{
    ActiveValue::Set, ColumnTrait, DatabaseConnection, DbErr, EntityTrait, QueryFilter, QueryOrder,
    QuerySelect, QueryTrait, SelectTwo, TransactionTrait, Value,
};

use crate::entity::document::{self, PageType};
use crate::entity::document_chunk;

const CANDIDATES_PER_RETRIEVER: u64 = 50;
const RECIPROCAL_RANK_FUSION_K: f64 = 60.0;

const LEXICAL_MATCH: &str = "to_tsvector('english', \"document_chunks\".\"content\") @@ to_tsquery('english', $1) \
     OR to_tsvector('english', \"documents\".\"title\") @@ to_tsquery('english', $2)";
const LEXICAL_RANK: &str = "ts_rank_cd(setweight(to_tsvector('english', \"documents\".\"title\"), 'A') \
     || setweight(to_tsvector('english', \"document_chunks\".\"content\"), 'B'), to_tsquery('english', $1))";
const SEMANTIC_DISTANCE: &str = "\"document_chunks\".\"embedding\" <=> $1";

pub struct DocumentWrite {
    pub document: document::ActiveModel,
    pub chunks: Vec<document_chunk::ActiveModel>,
}

pub type Passage = (document_chunk::Model, document::Model);

#[derive(Clone, Debug, PartialEq)]
pub struct Ranked {
    pub passage: Passage,
    pub score: f64,
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
    ) -> impl Future<Output = Result<Vec<Passage>, DbErr>> + Send;
}

pub fn fuse(ranked_lists: Vec<Vec<Passage>>) -> Vec<Ranked> {
    let mut fused: HashMap<i64, Ranked> = HashMap::new();
    for list in ranked_lists {
        for (index, passage) in list.into_iter().enumerate() {
            let contribution = 1.0 / (RECIPROCAL_RANK_FUSION_K + (index + 1) as f64);
            fused
                .entry(passage.0.id)
                .and_modify(|ranked| ranked.score += contribution)
                .or_insert(Ranked {
                    passage,
                    score: contribution,
                });
        }
    }
    let mut ranked: Vec<Ranked> = fused.into_values().collect();
    ranked.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.passage.0.id.cmp(&b.passage.0.id))
    });
    ranked
}

pub fn best_per_document(ranked: Vec<Ranked>, limit: usize) -> Vec<Ranked> {
    let mut seen = HashSet::new();
    ranked
        .into_iter()
        .filter(|item| seen.insert(item.passage.1.id))
        .take(limit)
        .collect()
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
        let distance = Expr::cust_with_values(
            SEMANTIC_DISTANCE,
            [Value::from(PgVector::from(query_embedding))],
        );
        passages(page_types)
            .order_by(distance, Order::Asc)
            .limit(CANDIDATES_PER_RETRIEVER)
            .all(&self.db)
            .await
            .and_then(passages_with_documents)
    }

    async fn lexical(
        &self,
        lexical_query: &str,
        page_types: Vec<PageType>,
    ) -> Result<Vec<Passage>, DbErr> {
        let query = || Value::from(lexical_query.to_owned());
        passages(page_types)
            .filter(Expr::cust_with_values(LEXICAL_MATCH, [query(), query()]))
            .order_by(Expr::cust_with_values(LEXICAL_RANK, [query()]), Order::Desc)
            .limit(CANDIDATES_PER_RETRIEVER)
            .all(&self.db)
            .await
            .and_then(passages_with_documents)
    }
}

fn passages(page_types: Vec<PageType>) -> SelectTwo<document_chunk::Entity, document::Entity> {
    document_chunk::Entity::find()
        .find_also_related(document::Entity)
        .apply_if(
            (!page_types.is_empty()).then_some(page_types),
            |select, page_types| select.filter(document::Column::PageType.is_in(page_types)),
        )
}

fn passages_with_documents(
    rows: Vec<(document_chunk::Model, Option<document::Model>)>,
) -> Result<Vec<Passage>, DbErr> {
    rows.into_iter()
        .map(|(chunk, document)| {
            document
                .map(|document| (chunk, document))
                .ok_or_else(|| DbErr::Custom("a passage has no document".to_owned()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use sea_orm::EntityTrait;

    use super::*;
    use crate::entity::document_chunk::EMBEDDING_DIMENSIONS;
    use crate::search;
    use crate::test_db;

    fn fixed_dimension_embedding(a: f32, b: f32, c: f32) -> Vec<f32> {
        let mut values = vec![0.0; EMBEDDING_DIMENSIONS as usize];
        values[0] = a;
        values[1] = b;
        values[2] = c;
        values
    }

    fn document(
        source_id: &str,
        page_type: PageType,
        chunks: Vec<(&str, Vec<f32>)>,
    ) -> DocumentWrite {
        let content = chunks
            .iter()
            .map(|(text, _)| *text)
            .collect::<Vec<_>>()
            .join("\n");
        DocumentWrite {
            document: document::ActiveModel {
                source: Set("crawler".to_owned()),
                source_id: Set(source_id.to_owned()),
                title: Set(format!("Title of {source_id}")),
                content: Set(content),
                content_hash: Set("0".repeat(64)),
                page_type: Set(page_type),
                keywords: Set(vec!["docs".to_owned()]),
                summary: Set("A summary.".to_owned()),
                embedding_model: Set("Qwen/Qwen3-Embedding-0.6B".to_owned()),
                ..Default::default()
            },
            chunks: chunks
                .into_iter()
                .map(|(text, embedding)| document_chunk::ActiveModel {
                    content: Set(text.to_owned()),
                    embedding: Set(PgVector::from(embedding)),
                    ..Default::default()
                })
                .collect(),
        }
    }

    fn contents(passages: &[Passage]) -> Vec<&str> {
        passages
            .iter()
            .map(|passage| passage.0.content.as_str())
            .collect()
    }

    fn lexical(query: &str) -> String {
        search::any_word_lexical_query(query).unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_new_source_and_source_id_has_no_existing_hash() {
        let test = test_db::start().await;
        let store = PgDocuments::new(test.db.clone());

        let existing = store
            .existing_hash("crawler", "https://example.com/a")
            .await
            .unwrap();

        assert_eq!(existing, None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn upserting_again_replaces_the_passages_but_keeps_the_first_ingested_at() {
        let test = test_db::start().await;
        let store = PgDocuments::new(test.db.clone());
        store
            .upsert(document(
                "https://example.com/a",
                PageType::Documentation,
                vec![
                    ("old one", fixed_dimension_embedding(1.0, 0.0, 0.0)),
                    ("old two", fixed_dimension_embedding(0.0, 1.0, 0.0)),
                ],
            ))
            .await
            .unwrap();
        let first_ingested_at =
            document::Entity::find().all(&test.db).await.unwrap()[0].ingested_at;

        store
            .upsert(document(
                "https://example.com/a",
                PageType::Documentation,
                vec![("new only", fixed_dimension_embedding(0.0, 0.0, 1.0))],
            ))
            .await
            .unwrap();

        let rows = document::Entity::find().all(&test.db).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].content, "new only");
        assert_eq!(rows[0].ingested_at, first_ingested_at);
        let chunks = document_chunk::Entity::find().all(&test.db).await.unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(
            (chunks[0].ordinal, chunks[0].content.as_str()),
            (0, "new only")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_passage_write_rolls_back_the_whole_upsert() {
        let test = test_db::start().await;
        let store = PgDocuments::new(test.db.clone());
        store
            .upsert(document(
                "https://example.com/a",
                PageType::Documentation,
                vec![("original passage", fixed_dimension_embedding(1.0, 0.0, 0.0))],
            ))
            .await
            .unwrap();

        let mut broken = document("https://example.com/a", PageType::Blog, vec![]);
        broken.document.content = Set("replacement".to_owned());
        broken.chunks = vec![document_chunk::ActiveModel {
            content: Set("wrong dimension".to_owned()),
            embedding: Set(PgVector::from(vec![1.0, 0.0, 0.0])),
            ..Default::default()
        }];
        assert!(store.upsert(broken).await.is_err());

        let rows = document::Entity::find().all(&test.db).await.unwrap();
        assert_eq!(
            (rows[0].content.as_str(), rows[0].page_type),
            ("original passage", PageType::Documentation)
        );
        let chunks = document_chunk::Entity::find().all(&test.db).await.unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].content, "original passage");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_document_can_be_stored_with_no_passages() {
        let test = test_db::start().await;
        let store = PgDocuments::new(test.db.clone());

        store
            .upsert(document("https://example.com/a", PageType::Other, vec![]))
            .await
            .unwrap();

        assert_eq!(
            document::Entity::find().all(&test.db).await.unwrap().len(),
            1
        );
        assert!(
            document_chunk::Entity::find()
                .all(&test.db)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_same_source_id_from_a_different_source_is_a_different_document() {
        let test = test_db::start().await;
        let store = PgDocuments::new(test.db.clone());
        let page = || {
            document(
                "https://example.com/a",
                PageType::Other,
                vec![("body", fixed_dimension_embedding(1.0, 0.0, 0.0))],
            )
        };

        store.upsert(page()).await.unwrap();
        let mut manual_page = page();
        manual_page.document.source = Set("manual".to_owned());
        store.upsert(manual_page).await.unwrap();

        assert_eq!(
            document::Entity::find().all(&test.db).await.unwrap().len(),
            2
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn nearest_ranks_passages_by_embedding_across_documents() {
        let test = test_db::start().await;
        let store = PgDocuments::new(test.db.clone());
        store
            .upsert(document(
                "https://example.com/a",
                PageType::Product,
                vec![
                    ("far passage", fixed_dimension_embedding(0.0, 1.0, 0.0)),
                    ("close passage", fixed_dimension_embedding(1.0, 0.0, 0.0)),
                ],
            ))
            .await
            .unwrap();
        store
            .upsert(document(
                "https://example.com/b",
                PageType::Blog,
                vec![("middle passage", fixed_dimension_embedding(0.7, 0.7, 0.0))],
            ))
            .await
            .unwrap();

        let nearest = store
            .nearest(fixed_dimension_embedding(1.0, 0.0, 0.0), vec![])
            .await
            .unwrap();

        assert_eq!(
            contents(&nearest),
            ["close passage", "middle passage", "far passage"]
        );
        assert_eq!(nearest[0].1.source_id, "https://example.com/a");
        assert_eq!(nearest[0].0.document_id, nearest[2].0.document_id);
        assert_eq!(nearest[0].1.page_type, PageType::Product);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn lexical_matches_any_word_in_passage_or_title_and_ranks_more_matches_first() {
        let test = test_db::start().await;
        let store = PgDocuments::new(test.db.clone());
        store
            .upsert(document(
                "https://example.com/a",
                PageType::Product,
                vec![
                    (
                        "Wireless noise cancelling headphones for travel.",
                        fixed_dimension_embedding(1.0, 0.0, 0.0),
                    ),
                    (
                        "A fifty foot garden hose.",
                        fixed_dimension_embedding(0.0, 1.0, 0.0),
                    ),
                    (
                        "Headphones with a long cable.",
                        fixed_dimension_embedding(0.0, 0.0, 1.0),
                    ),
                ],
            ))
            .await
            .unwrap();
        let mut titled = document(
            "https://example.com/titled",
            PageType::Product,
            vec![(
                "Pads and stands for your phone.",
                fixed_dimension_embedding(0.5, 0.5, 0.0),
            )],
        );
        titled.document.title = Set("Wireless charging".to_owned());
        store.upsert(titled).await.unwrap();

        let matched = store
            .lexical(&lexical("which wireless headphones"), vec![])
            .await
            .unwrap();

        let position = |text: &str| matched.iter().position(|passage| passage.0.content == text);
        assert!(
            position("Wireless noise cancelling headphones for travel.")
                < position("Headphones with a long cable.")
        );
        let mut found = contents(&matched);
        found.sort();
        assert_eq!(
            found,
            [
                "Headphones with a long cable.",
                "Pads and stands for your phone.",
                "Wireless noise cancelling headphones for travel."
            ]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_page_type_filter_restricts_both_retrievers() {
        let test = test_db::start().await;
        let store = PgDocuments::new(test.db.clone());
        store
            .upsert(document(
                "https://example.com/product",
                PageType::Product,
                vec![(
                    "headphones product page",
                    fixed_dimension_embedding(1.0, 0.0, 0.0),
                )],
            ))
            .await
            .unwrap();
        store
            .upsert(document(
                "https://example.com/blog",
                PageType::Blog,
                vec![(
                    "headphones blog review",
                    fixed_dimension_embedding(1.0, 0.0, 0.0),
                )],
            ))
            .await
            .unwrap();

        let nearest = store
            .nearest(
                fixed_dimension_embedding(1.0, 0.0, 0.0),
                vec![PageType::Blog],
            )
            .await
            .unwrap();
        let matched = store
            .lexical(
                &lexical("headphones"),
                vec![PageType::Blog, PageType::Knowledge],
            )
            .await
            .unwrap();

        assert_eq!(contents(&nearest), ["headphones blog review"]);
        assert_eq!(contents(&matched), ["headphones blog review"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_passage_found_only_by_its_embedding_still_surfaces_after_fusion() {
        let test = test_db::start().await;
        let store = PgDocuments::new(test.db.clone());
        store
            .upsert(document(
                "https://example.com/synonym",
                PageType::Product,
                vec![(
                    "Kabellose Kopfhörer mit Geräuschunterdrückung.",
                    fixed_dimension_embedding(1.0, 0.0, 0.0),
                )],
            ))
            .await
            .unwrap();

        let nearest = store
            .nearest(fixed_dimension_embedding(1.0, 0.0, 0.0), vec![])
            .await
            .unwrap();
        let matched = store
            .lexical(&lexical("noise cancelling headphones"), vec![])
            .await
            .unwrap();
        let ranked = fuse(vec![nearest, matched.clone()]);

        assert!(matched.is_empty());
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].passage.1.source_id, "https://example.com/synonym");
    }

    fn passage(chunk_id: i64, document_id: i64) -> Passage {
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
                source_id: format!("doc-{document_id}"),
                title: String::new(),
                content: String::new(),
                content_hash: "0".repeat(64),
                page_type: PageType::Other,
                keywords: vec![],
                summary: String::new(),
                embedding_model: String::new(),
                ingested_at: chrono::DateTime::UNIX_EPOCH,
                updated_at: chrono::DateTime::UNIX_EPOCH,
            },
        )
    }

    fn chunk_ids(ranked: &[Ranked]) -> Vec<i64> {
        ranked.iter().map(|item| item.passage.0.id).collect()
    }

    #[test]
    fn fuse_prefers_a_passage_both_retrievers_rank_over_one_only_one_ranks_first() {
        let ranked = fuse(vec![
            vec![passage(1, 1), passage(2, 2)],
            vec![passage(2, 2)],
        ]);

        assert_eq!(chunk_ids(&ranked), [2, 1]);
        assert!((ranked[0].score - (1.0 / 62.0 + 1.0 / 61.0)).abs() < 1e-12);
        assert!((ranked[1].score - 1.0 / 61.0).abs() < 1e-12);
    }

    #[test]
    fn fuse_breaks_ties_by_chunk_id_and_keeps_every_passage() {
        let list: Vec<Passage> = (1..=12).map(|n| passage(n, 1)).collect();
        let reversed: Vec<Passage> = list.iter().rev().cloned().collect();

        let ranked = fuse(vec![list, reversed]);

        assert_eq!(ranked.len(), 12);
        assert_eq!(chunk_ids(&ranked)[..2], [1, 12]);
    }

    #[test]
    fn fuse_of_nothing_is_nothing() {
        assert!(fuse(vec![vec![], vec![]]).is_empty());
    }

    #[test]
    fn best_per_document_keeps_each_documents_top_passage_in_order_up_to_the_limit() {
        let ranked = fuse(vec![vec![
            passage(1, 1),
            passage(2, 1),
            passage(3, 2),
            passage(4, 3),
        ]]);

        assert_eq!(chunk_ids(&best_per_document(ranked, 2)), [1, 3]);
    }
}
