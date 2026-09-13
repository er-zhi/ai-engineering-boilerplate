// Exercises bounded, revision-safe reads of complete documents.

use chrono::DateTime;
use common::proto::knowledge_base::v1::{DocumentRef, ReadDocumentRequest};

use super::super::*;
use super::support::{FakeLlm, MemoryDocuments};
use crate::entity::document::{self, PageType};

fn stored_document(content: &str) -> document::Model {
    document::Model {
        id: 7,
        source: "crawler".to_owned(),
        source_id: "https://example.com/guide".to_owned(),
        title: "Complete guide".to_owned(),
        content: content.to_owned(),
        content_hash: "a".repeat(64),
        page_type: PageType::Documentation,
        keywords: vec!["guide".to_owned()],
        summary: "Guide summary".to_owned(),
        embedding_model: "test".to_owned(),
        ingested_at: DateTime::UNIX_EPOCH,
        updated_at: DateTime::UNIX_EPOCH,
    }
}

fn request(cursor: &str, max_chars: u32) -> ReadDocumentRequest {
    ReadDocumentRequest {
        document: DocumentRef {
            source: "crawler".to_owned(),
            source_id: "https://example.com/guide".to_owned(),
            version: "a".repeat(64),
            ..Default::default()
        }
        .into(),
        cursor: cursor.to_owned(),
        max_chars,
        ..Default::default()
    }
}

#[tokio::test]
async fn pages_preserve_unicode_and_reassemble_the_exact_document() {
    let documents = MemoryDocuments {
        document: Some(stored_document("one界two🙂three")),
        ..MemoryDocuments::default()
    };
    let knowledge_base = KnowledgeBase::new(documents, FakeLlm::default());

    let first = knowledge_base.read_document(request("", 5)).await.unwrap();
    let second = knowledge_base
        .read_document(request(&first.next_cursor, 50))
        .await
        .unwrap();

    assert_eq!(
        format!("{}{}", first.content, second.content),
        "one界two🙂three"
    );
    assert_eq!(first.total_chars, 13);
    assert!(!first.next_cursor.is_empty());
    assert!(second.next_cursor.is_empty());
    assert_eq!(second.document.version, "a".repeat(64));
}

#[tokio::test]
async fn a_changed_or_missing_document_is_reported_explicitly() {
    let knowledge_base = KnowledgeBase::new(
        MemoryDocuments {
            document: Some(stored_document("body")),
            ..MemoryDocuments::default()
        },
        FakeLlm::default(),
    );
    let mut stale = request("", 10);
    stale
        .document
        .modify(|reference| reference.version = "b".repeat(64));

    let changed = knowledge_base.read_document(stale).await.unwrap_err();
    let missing = KnowledgeBase::new(MemoryDocuments::default(), FakeLlm::default())
        .read_document(request("", 10))
        .await
        .unwrap_err();

    assert!(format!("{changed:?}").contains("changed"), "{changed:?}");
    assert!(format!("{missing:?}").contains("not found"), "{missing:?}");
}

#[tokio::test]
async fn invalid_references_cursors_and_page_sizes_fail_before_returning_content() {
    let knowledge_base = KnowledgeBase::new(
        MemoryDocuments {
            document: Some(stored_document("body")),
            ..MemoryDocuments::default()
        },
        FakeLlm::default(),
    );
    let mut missing_reference = request("", 10);
    missing_reference.document = Default::default();
    let mut bad_version = request("", 10);
    bad_version
        .document
        .modify(|reference| reference.version = "not-a-hash".to_owned());

    for invalid in [
        missing_reference,
        bad_version,
        request("not-a-cursor", 10),
        request("", 50_001),
        request("kb1:ffff", 10),
    ] {
        assert!(knowledge_base.read_document(invalid).await.is_err());
    }
}

#[tokio::test]
async fn a_store_failure_is_unavailable() {
    let knowledge_base = KnowledgeBase::new(
        MemoryDocuments {
            fails: true,
            ..MemoryDocuments::default()
        },
        FakeLlm::default(),
    );

    let error = knowledge_base
        .read_document(request("", 10))
        .await
        .unwrap_err();

    assert!(format!("{error:?}").contains("document store"), "{error:?}");
}
