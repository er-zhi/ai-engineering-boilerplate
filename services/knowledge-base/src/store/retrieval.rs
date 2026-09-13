// Builds and executes bounded semantic, passage-text, and title-text retrieval queries.

use sea_orm::entity::prelude::PgVector;
use sea_orm::sea_query::{Expr, Order};
use sea_orm::{
    ColumnTrait, DatabaseConnection, DbErr, EntityTrait, JoinType, QueryFilter, QueryOrder,
    QuerySelect, QueryTrait, RelationTrait, Select, SelectModel, Selector, Value,
};

use super::Passage;
use crate::entity::document::{self, PageType};
use crate::entity::document_chunk;

const CANDIDATES_PER_RETRIEVER: u64 = 50;
const PASSAGE_LEXICAL_MATCH: &str =
    "to_tsvector('english', \"document_chunks\".\"content\") @@ to_tsquery('english', $1)";
const PASSAGE_LEXICAL_RANK: &str = "ts_rank_cd(to_tsvector('english', \"document_chunks\".\"content\"), to_tsquery('english', $1))";
const TITLE_LEXICAL_MATCH: &str =
    "to_tsvector('english', \"documents\".\"title\") @@ to_tsquery('english', $1)";
const TITLE_LEXICAL_RANK: &str =
    "ts_rank_cd(to_tsvector('english', \"documents\".\"title\"), to_tsquery('english', $1))";
const SEMANTIC_DISTANCE: &str = "\"document_chunks\".\"embedding\" <=> $1";

pub(super) async fn nearest(
    db: &DatabaseConnection,
    query_embedding: Vec<f32>,
    page_types: Vec<PageType>,
) -> Result<Vec<Passage>, DbErr> {
    let distance = Expr::cust_with_values(
        SEMANTIC_DISTANCE,
        [Value::from(PgVector::from(query_embedding))],
    );
    projected_passages(
        passages(page_types)
            .order_by(distance, Order::Asc)
            .limit(CANDIDATES_PER_RETRIEVER),
    )
    .all(db)
    .await
}

pub(super) async fn lexical_passages(
    db: &DatabaseConnection,
    lexical_query: &str,
    page_types: Vec<PageType>,
) -> Result<Vec<Passage>, DbErr> {
    projected_passages(lexical_passage_candidates(lexical_query, page_types))
        .all(db)
        .await
}

pub(super) async fn lexical_titles(
    db: &DatabaseConnection,
    lexical_query: &str,
    page_types: Vec<PageType>,
) -> Result<Vec<Passage>, DbErr> {
    projected_passages(lexical_title_candidates(lexical_query, page_types))
        .all(db)
        .await
}

pub(super) fn lexical_passage_candidates(
    lexical_query: &str,
    page_types: Vec<PageType>,
) -> Select<document_chunk::Entity> {
    let query = || Value::from(lexical_query.to_owned());
    passages(page_types)
        .filter(Expr::cust_with_values(PASSAGE_LEXICAL_MATCH, [query()]))
        .order_by(
            Expr::cust_with_values(PASSAGE_LEXICAL_RANK, [query()]),
            Order::Desc,
        )
        .order_by(document_chunk::Column::Id, Order::Asc)
        .limit(CANDIDATES_PER_RETRIEVER)
}

pub(super) fn lexical_title_candidates(
    lexical_query: &str,
    page_types: Vec<PageType>,
) -> Select<document_chunk::Entity> {
    let query = || Value::from(lexical_query.to_owned());
    passages(page_types)
        .filter(document_chunk::Column::Ordinal.eq(0))
        .filter(Expr::cust_with_values(TITLE_LEXICAL_MATCH, [query()]))
        .order_by(
            Expr::cust_with_values(TITLE_LEXICAL_RANK, [query()]),
            Order::Desc,
        )
        .order_by(document::Column::Id, Order::Asc)
        .limit(CANDIDATES_PER_RETRIEVER)
}

fn passages(page_types: Vec<PageType>) -> Select<document_chunk::Entity> {
    document_chunk::Entity::find()
        .join(
            JoinType::InnerJoin,
            document_chunk::Relation::Document.def(),
        )
        .apply_if(
            (!page_types.is_empty()).then_some(page_types),
            |select, page_types| select.filter(document::Column::PageType.is_in(page_types)),
        )
}

fn projected_passages(query: Select<document_chunk::Entity>) -> Selector<SelectModel<Passage>> {
    passage_projection(query).into_model::<Passage>()
}

pub(super) fn passage_projection(
    query: Select<document_chunk::Entity>,
) -> Select<document_chunk::Entity> {
    query
        .select_only()
        .column_as(document_chunk::Column::Id, "chunk_id")
        .column_as(document_chunk::Column::DocumentId, "document_id")
        .column(document_chunk::Column::Content)
        .column(document::Column::Source)
        .column(document::Column::SourceId)
        .column(document::Column::Title)
        .column(document::Column::Summary)
        .column(document::Column::PageType)
        .column(document::Column::Keywords)
        .column(document::Column::UpdatedAt)
}
