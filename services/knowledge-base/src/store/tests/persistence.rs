// Exercises document identity, transactional replacement, and rollback behavior.

use sea_orm::entity::prelude::PgVector;
use sea_orm::{ActiveValue::Set, EntityTrait};

use super::support::*;
use crate::entity::document::PageType;
use crate::entity::{document, document_chunk};
use crate::store::{DocumentStore, PgDocuments};
use crate::test_db;

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
async fn a_document_can_be_read_by_its_external_identity() {
    let test = test_db::start().await;
    let store = PgDocuments::new(test.db.clone());
    store
        .upsert(document_write(
            "https://example.com/a",
            PageType::Documentation,
            vec![("complete body", fixed_dimension_embedding(1.0, 0.0, 0.0))],
        ))
        .await
        .unwrap();

    let found = store
        .document("crawler", "https://example.com/a")
        .await
        .unwrap()
        .unwrap();

    assert_eq!(found.content, "complete body");
    assert_eq!(found.content_hash, "0".repeat(64));
}

#[tokio::test(flavor = "multi_thread")]
async fn upserting_again_replaces_the_passages_but_keeps_the_first_ingested_at() {
    let test = test_db::start().await;
    let store = PgDocuments::new(test.db.clone());
    store
        .upsert(document_write(
            "https://example.com/a",
            PageType::Documentation,
            vec![
                ("old one", fixed_dimension_embedding(1.0, 0.0, 0.0)),
                ("old two", fixed_dimension_embedding(0.0, 1.0, 0.0)),
            ],
        ))
        .await
        .unwrap();
    let first_ingested_at = document::Entity::find().all(&test.db).await.unwrap()[0].ingested_at;

    store
        .upsert(document_write(
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
        .upsert(document_write(
            "https://example.com/a",
            PageType::Documentation,
            vec![("original passage", fixed_dimension_embedding(1.0, 0.0, 0.0))],
        ))
        .await
        .unwrap();

    let mut broken = document_write("https://example.com/a", PageType::Blog, vec![]);
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
        .upsert(document_write(
            "https://example.com/a",
            PageType::Other,
            vec![],
        ))
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
        document_write(
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
