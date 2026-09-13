// Exercises reciprocal-rank fusion and document deduplication.

use crate::entity::document::PageType;
use crate::store::{Passage, Ranked, best_per_document, fuse, fuse_with_document_titles};

fn passage(chunk_id: i64, document_id: i64) -> Passage {
    Passage {
        chunk_id,
        document_id,
        ordinal: 0,
        content: format!("passage {chunk_id}"),
        source: "crawler".to_owned(),
        source_id: format!("doc-{document_id}"),
        title: String::new(),
        summary: String::new(),
        page_type: PageType::Other,
        keywords: vec![],
        updated_at: chrono::DateTime::UNIX_EPOCH,
        content_hash: "0".repeat(64),
    }
}

fn chunk_ids(ranked: &[Ranked]) -> Vec<i64> {
    ranked.iter().map(|item| item.passage.chunk_id).collect()
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

#[test]
fn a_title_match_boosts_the_documents_best_retrieved_passage_and_only_falls_back_when_needed() {
    let retrieved = vec![passage(2, 10), passage(3, 20)];
    let titles = vec![passage(4, 30), passage(1, 10)];

    let ranked = fuse_with_document_titles(vec![retrieved], titles);

    assert_eq!(chunk_ids(&ranked), [2, 4, 3]);
    assert!(ranked[0].score > ranked[2].score);
    assert!(!chunk_ids(&ranked).contains(&1));
}
