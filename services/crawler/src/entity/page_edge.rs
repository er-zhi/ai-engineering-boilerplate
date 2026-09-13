// crawler.page_edges: one row per link discovered from one page to another, keyed by URL rather than a page id.

use sea_orm::entity::prelude::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "i16", db_type = "SmallInteger")]
pub enum RelationType {
    #[sea_orm(num_value = 0)]
    LinksTo,
    #[sea_orm(num_value = 1)]
    Parent,
    #[sea_orm(num_value = 2)]
    Canonical,
    #[sea_orm(num_value = 3)]
    Redirect,
}

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "page_edges", schema_name = "crawler")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(
        unique_key = "from_to_relation",
        column_type = "String(StringLen::N(crate::links::MAX_URL_BYTES as u32))"
    )]
    pub from_url: String,
    #[sea_orm(
        unique_key = "from_to_relation",
        column_type = "String(StringLen::N(crate::links::MAX_URL_BYTES as u32))"
    )]
    pub to_url: String,
    #[sea_orm(unique_key = "from_to_relation")]
    pub relation_type: RelationType,
    #[sea_orm(column_type = "String(StringLen::N(crate::links::MAX_ANCHOR_TEXT_CHARS as u32))")]
    pub anchor_text: String,
    #[sea_orm(column_type = "Json", nullable)]
    pub metadata: Option<Json>,
    pub discovered_at: DateTimeUtc,
}

impl ActiveModelBehavior for ActiveModel {}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use sea_orm::ActiveValue::Set;

    use super::*;
    use crate::test_db;

    fn valid_row(from_url: &str, to_url: &str) -> ActiveModel {
        ActiveModel {
            from_url: Set(from_url.to_owned()),
            to_url: Set(to_url.to_owned()),
            relation_type: Set(RelationType::LinksTo),
            anchor_text: Set("Docs".to_owned()),
            metadata: Set(None),
            discovered_at: Set(Utc::now()),
            ..Default::default()
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_same_from_to_relation_triple_is_rejected_twice() {
        let test = test_db::start().await;

        valid_row("https://example.com/a", "https://example.com/b")
            .insert(&test.db)
            .await
            .unwrap();

        assert!(
            valid_row("https://example.com/a", "https://example.com/b")
                .insert(&test.db)
                .await
                .is_err(),
            "duplicate (from_url, to_url, relation_type) accepted"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_same_urls_with_a_different_relation_type_are_a_different_edge() {
        let test = test_db::start().await;
        valid_row("https://example.com/a", "https://example.com/b")
            .insert(&test.db)
            .await
            .unwrap();

        let other_relation = ActiveModel {
            relation_type: Set(RelationType::Canonical),
            ..valid_row("https://example.com/a", "https://example.com/b")
        };

        assert!(other_relation.insert(&test.db).await.is_ok());
    }
}
