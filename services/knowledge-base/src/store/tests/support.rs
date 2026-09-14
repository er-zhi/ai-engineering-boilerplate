// Provides bounded document and embedding fixtures for store tests.

use sea_orm::ActiveValue::Set;
use sea_orm::entity::prelude::PgVector;

use crate::entity::document::{self, PageType};
use crate::entity::document_chunk::{self, EMBEDDING_DIMENSIONS};
use crate::search;
use crate::store::{DocumentStore, DocumentWrite, Passage, PgDocuments};

pub(super) fn fixed_dimension_embedding(a: f32, b: f32, c: f32) -> Vec<f32> {
    let mut values = vec![0.0; EMBEDDING_DIMENSIONS as usize];
    values[0] = a;
    values[1] = b;
    values[2] = c;
    values
}

pub(super) fn document_write(
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
            embedding_model: Set("google/embeddinggemma-300m".to_owned()),
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

pub(super) fn contents(passages: &[Passage]) -> Vec<&str> {
    passages
        .iter()
        .map(|passage| passage.content.as_str())
        .collect()
}

pub(super) fn lexical(query: &str) -> String {
    search::any_word_lexical_query(query).unwrap()
}

pub(super) async fn insert_lexical_fixtures(store: &PgDocuments) {
    store
        .upsert(document_write(
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
    let many_chunks = (0..51)
        .map(|_| {
            (
                "Pads and stands for your phone.",
                fixed_dimension_embedding(0.5, 0.5, 0.0),
            )
        })
        .collect();
    let mut titled = document_write("https://example.com/titled", PageType::Product, many_chunks);
    titled.document.title = Set("Wireless wireless charging".to_owned());
    store.upsert(titled).await.unwrap();
    let mut second_title = document_write(
        "https://example.com/second-title",
        PageType::Product,
        vec![(
            "A second title match.",
            fixed_dimension_embedding(0.5, 0.5, 0.0),
        )],
    );
    second_title.document.title = Set("Wireless charging".to_owned());
    store.upsert(second_title).await.unwrap();
}
