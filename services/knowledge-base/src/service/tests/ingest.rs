// Exercises ingest request validation and service orchestration.

use super::super::*;
use super::support::*;

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
