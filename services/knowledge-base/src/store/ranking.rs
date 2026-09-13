// Fuses independent retrieval rankings and keeps the best passage per document.

use std::collections::{HashMap, HashSet};

use super::{Passage, Ranked};

const RECIPROCAL_RANK_FUSION_K: f64 = 60.0;

pub fn fuse(ranked_lists: Vec<Vec<Passage>>) -> Vec<Ranked> {
    let mut fused: HashMap<i64, Ranked> = HashMap::new();
    for list in ranked_lists {
        for (index, passage) in list.into_iter().enumerate() {
            let contribution = 1.0 / (RECIPROCAL_RANK_FUSION_K + (index + 1) as f64);
            fused
                .entry(passage.chunk_id)
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
            .then_with(|| a.passage.chunk_id.cmp(&b.passage.chunk_id))
    });
    ranked
}

pub fn best_per_document(ranked: Vec<Ranked>, limit: usize) -> Vec<Ranked> {
    let mut seen = HashSet::new();
    ranked
        .into_iter()
        .filter(|item| seen.insert(item.passage.document_id))
        .take(limit)
        .collect()
}

pub fn fuse_with_document_titles(
    passage_lists: Vec<Vec<Passage>>,
    title_passages: Vec<Passage>,
) -> Vec<Ranked> {
    let mut ranked = fuse(passage_lists);
    let mut title_scores = HashMap::new();
    let mut fallbacks = HashMap::new();
    for (index, passage) in title_passages.into_iter().enumerate() {
        let score = 1.0 / (RECIPROCAL_RANK_FUSION_K + (index + 1) as f64);
        title_scores.insert(passage.document_id, score);
        fallbacks.insert(passage.document_id, (score, passage));
    }
    for item in &mut ranked {
        if let Some(score) = title_scores.get(&item.passage.document_id) {
            item.score += score;
            fallbacks.remove(&item.passage.document_id);
        }
    }
    ranked.extend(
        fallbacks
            .into_values()
            .map(|(score, passage)| Ranked { passage, score }),
    );
    ranked.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.passage.chunk_id.cmp(&b.passage.chunk_id))
    });
    ranked
}
