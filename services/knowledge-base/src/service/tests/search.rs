// Exercises search validation, fusion, limits, and failure mapping.

use super::super::*;
use super::support::*;

#[tokio::test]
async fn a_search_fuses_passages_and_returns_each_document_once_with_its_best_passage() {
    let documents = MemoryDocuments {
        nearest: vec![passage(1, "a"), passage(2, "b"), passage(3, "a")],
        lexical_passages: vec![passage(2, "b"), passage(3, "a")],
        lexical_titles: vec![passage(2, "b")],
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
    assert_eq!(response.results[0].document.source_id, "b");
    assert_eq!(response.results[0].document.version, "0".repeat(64));
    assert_eq!(response.results[0].passage_ordinal, 0);
    assert!(response.results[0].score > response.results[1].score);

    let wanted = vec![PageType::Product, PageType::Blog];
    assert_eq!(
        documents.nearest_with.lock().unwrap()[0],
        (vec![0.1, 0.2, 0.3], wanted.clone())
    );
    assert_eq!(
        documents.lexical_passages_with.lock().unwrap()[0],
        ("wireless | product".to_owned(), wanted.clone())
    );
    assert_eq!(
        documents.lexical_titles_with.lock().unwrap()[0],
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
    assert!(documents.lexical_passages_with.lock().unwrap().is_empty());
    assert!(documents.lexical_titles_with.lock().unwrap().is_empty());
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
    assert!(documents.lexical_passages_with.lock().unwrap().is_empty());
    assert!(documents.lexical_titles_with.lock().unwrap().is_empty());
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
    assert_eq!(documents.lexical_passages_with.lock().unwrap().len(), 1);
    assert_eq!(documents.lexical_titles_with.lock().unwrap().len(), 1);
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
