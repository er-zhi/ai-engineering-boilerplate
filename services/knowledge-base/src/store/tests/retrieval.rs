// Exercises semantic and lexical retrieval against PostgreSQL.

use std::collections::HashSet;

use sea_orm::{DatabaseBackend, QueryTrait};

use super::support::*;
use crate::entity::document::PageType;
use crate::store::{DocumentStore, PgDocuments, fuse};
use crate::store::{lexical_passage_candidates, lexical_title_candidates, passage_projection};
use crate::test_db;

#[test]
fn lexical_queries_stay_separate_and_use_the_compact_search_projection() {
    let passage_sql = passage_projection(lexical_passage_candidates("agent | tool", vec![]))
        .build(DatabaseBackend::Postgres)
        .to_string();
    let title_sql = passage_projection(lexical_title_candidates("agent | tool", vec![]))
        .build(DatabaseBackend::Postgres)
        .to_string();

    assert!(passage_sql.contains("document_chunks\".\"content\") @@"));
    assert!(!passage_sql.contains("documents\".\"title\") @@"));
    assert!(title_sql.contains("documents\".\"title\") @@"));
    assert!(!title_sql.contains("document_chunks\".\"content\") @@"));
    assert!(title_sql.contains("document_chunks\".\"ordinal\" = 0"));
    for sql in [&passage_sql, &title_sql] {
        assert!(!sql.contains("document_chunks\".\"embedding\" AS"));
        assert!(!sql.contains("documents\".\"content\" AS"));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn nearest_ranks_passages_by_embedding_across_documents() {
    let test = test_db::start().await;
    let store = PgDocuments::new(test.db.clone());
    store
        .upsert(document_write(
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
        .upsert(document_write(
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
    assert_eq!(nearest[0].source_id, "https://example.com/a");
    assert_eq!(nearest[0].document_id, nearest[2].document_id);
    assert_eq!(nearest[0].page_type, PageType::Product);
}

#[tokio::test(flavor = "multi_thread")]
async fn lexical_retrievers_match_passages_and_titles_independently() {
    let test = test_db::start().await;
    let store = PgDocuments::new(test.db.clone());
    insert_lexical_fixtures(&store).await;

    let matched = store
        .lexical(&lexical("which wireless headphones"), vec![])
        .await
        .unwrap();
    let matched_passages = matched.passages;
    let matched_titles = matched.titles;

    let position = |text: &str| {
        matched_passages
            .iter()
            .position(|passage| passage.content == text)
    };
    assert!(
        position("Wireless noise cancelling headphones for travel.")
            < position("Headphones with a long cable.")
    );
    let mut found = contents(&matched_passages);
    found.sort();
    assert_eq!(
        found,
        [
            "Headphones with a long cable.",
            "Wireless noise cancelling headphones for travel."
        ]
    );
    assert_eq!(matched_titles.len(), 2);
    assert_eq!(
        matched_titles
            .iter()
            .map(|passage| passage.document_id)
            .collect::<HashSet<_>>()
            .len(),
        2
    );
    assert_eq!(
        contents(&matched_titles),
        ["Pads and stands for your phone.", "A second title match."]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_page_type_filter_restricts_both_retrievers() {
    let test = test_db::start().await;
    let store = PgDocuments::new(test.db.clone());
    store
        .upsert(document_write(
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
        .upsert(document_write(
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
    let matched_passages = matched.passages;
    let matched_titles = matched.titles;

    assert_eq!(contents(&nearest), ["headphones blog review"]);
    assert_eq!(contents(&matched_passages), ["headphones blog review"]);
    assert!(matched_titles.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_passage_found_only_by_its_embedding_still_surfaces_after_fusion() {
    let test = test_db::start().await;
    let store = PgDocuments::new(test.db.clone());
    store
        .upsert(document_write(
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
        .unwrap()
        .passages;
    let ranked = fuse(vec![nearest, matched.clone()]);

    assert!(matched.is_empty());
    assert_eq!(ranked.len(), 1);
    assert_eq!(ranked[0].passage.source_id, "https://example.com/synonym");
}
