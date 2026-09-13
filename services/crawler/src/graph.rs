// Page-to-page link topology: replacing one page's outbound edges wholesale on every crawl, and answering "what's reachable from this URL" through a bounded-depth breadth-first walk.

use std::collections::HashMap;

use chrono::Utc;
use sea_orm::{
    ActiveValue::Set, ColumnTrait, DatabaseConnection, DbErr, EntityTrait, FromQueryResult,
    QueryFilter, QueryOrder, QuerySelect, TransactionTrait,
};

use crate::entity::page;
use crate::entity::page_edge::{self, RelationType};
use crate::links::Link;

pub const MAX_REQUESTABLE_DEPTH: u32 = 3;
const MAX_RETURNED_NEIGHBORS: usize = 200;
const MAX_FRONTIER_ROWS_PER_HOP: u64 = 5_000;

#[derive(Clone, Debug, PartialEq, FromQueryResult)]
pub struct Neighbor {
    pub url: String,
    pub page_id: Option<i64>,
    pub title: Option<String>,
    pub relation_type: RelationType,
    pub depth: i32,
}

#[derive(Clone, Debug, FromQueryResult)]
struct EdgeRow {
    to_url: String,
    relation_type: RelationType,
}

pub trait EdgeStore: Clone + Send + Sync + 'static {
    fn replace_outbound(
        &self,
        from_url: &str,
        links: Vec<Link>,
    ) -> impl Future<Output = Result<(), DbErr>> + Send;

    fn neighbors(
        &self,
        start_url: &str,
        relation_types: Vec<RelationType>,
        max_depth: u32,
    ) -> impl Future<Output = Result<Vec<Neighbor>, DbErr>> + Send;
}

#[derive(Clone)]
pub struct PgEdges {
    db: DatabaseConnection,
}

impl PgEdges {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    async fn edges_from(
        &self,
        from_urls: &[String],
        relation_types: &[RelationType],
    ) -> Result<Vec<EdgeRow>, DbErr> {
        if from_urls.is_empty() {
            return Ok(Vec::new());
        }
        let mut query = page_edge::Entity::find()
            .select_only()
            .column(page_edge::Column::ToUrl)
            .column(page_edge::Column::RelationType)
            .filter(page_edge::Column::FromUrl.is_in(from_urls.iter().cloned()))
            .order_by_asc(page_edge::Column::ToUrl)
            .order_by_asc(page_edge::Column::RelationType)
            .limit(MAX_FRONTIER_ROWS_PER_HOP);
        if !relation_types.is_empty() {
            query =
                query.filter(page_edge::Column::RelationType.is_in(relation_types.iter().copied()));
        }
        query.into_model::<EdgeRow>().all(&self.db).await
    }

    async fn pages_by_url(&self, urls: &[String]) -> Result<HashMap<String, page::Model>, DbErr> {
        if urls.is_empty() {
            return Ok(HashMap::new());
        }
        let rows = page::Entity::find()
            .filter(page::Column::Url.is_in(urls.iter().cloned()))
            .all(&self.db)
            .await?;
        Ok(rows.into_iter().map(|row| (row.url.clone(), row)).collect())
    }
}

impl EdgeStore for PgEdges {
    async fn replace_outbound(&self, from_url: &str, links: Vec<Link>) -> Result<(), DbErr> {
        let txn = self.db.begin().await?;
        page_edge::Entity::delete_many()
            .filter(page_edge::Column::FromUrl.eq(from_url))
            .filter(page_edge::Column::RelationType.eq(RelationType::LinksTo))
            .exec(&txn)
            .await?;
        if !links.is_empty() {
            let discovered_at = Utc::now();
            let rows = links.into_iter().map(|link| page_edge::ActiveModel {
                from_url: Set(from_url.to_owned()),
                to_url: Set(link.url),
                relation_type: Set(RelationType::LinksTo),
                anchor_text: Set(link.anchor_text),
                metadata: Set(None),
                discovered_at: Set(discovered_at),
                ..Default::default()
            });
            page_edge::Entity::insert_many(rows).exec(&txn).await?;
        }
        txn.commit().await
    }

    async fn neighbors(
        &self,
        start_url: &str,
        relation_types: Vec<RelationType>,
        max_depth: u32,
    ) -> Result<Vec<Neighbor>, DbErr> {
        let mut shallowest: HashMap<String, (i32, RelationType)> = HashMap::new();
        let mut frontier = vec![start_url.to_owned()];

        for depth in 1..=max_depth as i32 {
            if frontier.is_empty() {
                break;
            }
            let edges = self.edges_from(&frontier, &relation_types).await?;
            frontier = Vec::new();
            for edge in edges {
                if edge.to_url == start_url || shallowest.contains_key(&edge.to_url) {
                    continue;
                }
                shallowest.insert(edge.to_url.clone(), (depth, edge.relation_type));
                frontier.push(edge.to_url);
            }
        }

        let mut ranked: Vec<(String, i32, RelationType)> = shallowest
            .into_iter()
            .map(|(url, (depth, relation_type))| (url, depth, relation_type))
            .collect();
        ranked.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
        ranked.truncate(MAX_RETURNED_NEIGHBORS);

        let urls: Vec<String> = ranked.iter().map(|(url, ..)| url.clone()).collect();
        let pages = self.pages_by_url(&urls).await?;

        Ok(ranked
            .into_iter()
            .map(|(url, depth, relation_type)| {
                let page = pages.get(&url);
                Neighbor {
                    page_id: page.map(|found| found.id),
                    title: page.map(|found| found.title.clone()),
                    url,
                    relation_type,
                    depth,
                }
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use sea_orm::ActiveModelTrait;
    use sea_orm::ActiveValue::Set;

    use super::*;
    use crate::entity::page;
    use crate::test_db;

    fn link(url: &str) -> Link {
        Link {
            url: url.to_owned(),
            anchor_text: "Link".to_owned(),
        }
    }

    async fn crawled(db: &DatabaseConnection, url: &str, title: &str) {
        page::ActiveModel {
            url: Set(url.to_owned()),
            title: Set(title.to_owned()),
            main_text: Set(String::new()),
            content_hash: Set("0".repeat(64)),
            http_status: Set(200),
            crawled_at: Set(Utc::now()),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap();
    }

    fn urls(neighbors: &[Neighbor]) -> Vec<&str> {
        neighbors.iter().map(|n| n.url.as_str()).collect()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn replacing_outbound_edges_drops_links_no_longer_on_the_page() {
        let test = test_db::start().await;
        let store = PgEdges::new(test.db.clone());
        store
            .replace_outbound("https://example.com/a", vec![link("https://example.com/b")])
            .await
            .unwrap();

        store
            .replace_outbound("https://example.com/a", vec![link("https://example.com/c")])
            .await
            .unwrap();

        let neighbors = store
            .neighbors("https://example.com/a", vec![], MAX_REQUESTABLE_DEPTH)
            .await
            .unwrap();
        assert_eq!(neighbors.len(), 1);
        assert_eq!(neighbors[0].url, "https://example.com/c");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn depth_limits_how_far_traversal_follows_the_chain() {
        let test = test_db::start().await;
        let store = PgEdges::new(test.db.clone());
        store
            .replace_outbound("https://example.com/a", vec![link("https://example.com/b")])
            .await
            .unwrap();
        store
            .replace_outbound("https://example.com/b", vec![link("https://example.com/c")])
            .await
            .unwrap();
        store
            .replace_outbound("https://example.com/c", vec![link("https://example.com/d")])
            .await
            .unwrap();

        let one_hop = store
            .neighbors("https://example.com/a", vec![], 1)
            .await
            .unwrap();
        let two_hops = store
            .neighbors("https://example.com/a", vec![], 2)
            .await
            .unwrap();
        let three_hops = store
            .neighbors("https://example.com/a", vec![], 3)
            .await
            .unwrap();

        assert_eq!(urls(&one_hop), ["https://example.com/b"]);
        assert_eq!(
            urls(&two_hops),
            ["https://example.com/b", "https://example.com/c"]
        );
        assert_eq!(
            urls(&three_hops),
            [
                "https://example.com/b",
                "https://example.com/c",
                "https://example.com/d"
            ]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_uncrawled_neighbor_has_no_page_id_or_title() {
        let test = test_db::start().await;
        let store = PgEdges::new(test.db.clone());
        store
            .replace_outbound(
                "https://example.com/a",
                vec![link("https://example.com/never-crawled")],
            )
            .await
            .unwrap();

        let neighbors = store
            .neighbors("https://example.com/a", vec![], MAX_REQUESTABLE_DEPTH)
            .await
            .unwrap();

        assert_eq!(neighbors.len(), 1);
        assert_eq!(neighbors[0].page_id, None);
        assert_eq!(neighbors[0].title, None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_crawled_neighbor_carries_its_page_id_and_title() {
        let test = test_db::start().await;
        crawled(&test.db, "https://example.com/b", "Page B").await;
        let store = PgEdges::new(test.db.clone());
        store
            .replace_outbound("https://example.com/a", vec![link("https://example.com/b")])
            .await
            .unwrap();

        let neighbors = store
            .neighbors("https://example.com/a", vec![], MAX_REQUESTABLE_DEPTH)
            .await
            .unwrap();

        assert_eq!(neighbors.len(), 1);
        assert!(neighbors[0].page_id.is_some());
        assert_eq!(neighbors[0].title.as_deref(), Some("Page B"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn replacing_outbound_links_to_edges_preserves_other_relation_types() {
        let test = test_db::start().await;
        let store = PgEdges::new(test.db.clone());
        for preserved in [
            RelationType::Canonical,
            RelationType::Parent,
            RelationType::Redirect,
        ] {
            page_edge::ActiveModel {
                from_url: Set("https://example.com/a".to_owned()),
                to_url: Set("https://example.com/preserved-target".to_owned()),
                relation_type: Set(preserved),
                anchor_text: Set(String::new()),
                metadata: Set(None),
                discovered_at: Set(Utc::now()),
                ..Default::default()
            }
            .insert(&test.db)
            .await
            .unwrap();
            store
                .replace_outbound("https://example.com/a", vec![link("https://example.com/b")])
                .await
                .unwrap();

            store
                .replace_outbound("https://example.com/a", vec![link("https://example.com/c")])
                .await
                .unwrap();

            let preserved_only = store
                .neighbors(
                    "https://example.com/a",
                    vec![preserved],
                    MAX_REQUESTABLE_DEPTH,
                )
                .await
                .unwrap();
            assert_eq!(
                urls(&preserved_only),
                ["https://example.com/preserved-target"],
                "{preserved:?} edge did not survive a LinksTo replace_outbound"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn neighbors_are_truncated_to_the_returned_cap_shallowest_first() {
        let test = test_db::start().await;
        let store = PgEdges::new(test.db.clone());
        let extra = MAX_RETURNED_NEIGHBORS + 10;
        let links: Vec<Link> = (0..extra)
            .map(|n| link(&format!("https://example.com/page-{n:04}")))
            .collect();
        store
            .replace_outbound("https://example.com/a", links)
            .await
            .unwrap();

        let neighbors = store
            .neighbors("https://example.com/a", vec![], MAX_REQUESTABLE_DEPTH)
            .await
            .unwrap();

        assert_eq!(neighbors.len(), MAX_RETURNED_NEIGHBORS);
        assert!(neighbors.iter().all(|n| n.depth == 1));
        let mut sorted_urls: Vec<&str> = urls(&neighbors);
        sorted_urls.sort_unstable();
        assert_eq!(
            urls(&neighbors),
            sorted_urls,
            "expected URL-ascending order"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_relation_type_filter_excludes_other_relations() {
        let test = test_db::start().await;
        let store = PgEdges::new(test.db.clone());
        let discovered_at = Utc::now();
        page_edge::ActiveModel {
            from_url: Set("https://example.com/a".to_owned()),
            to_url: Set("https://example.com/canonical-target".to_owned()),
            relation_type: Set(RelationType::Canonical),
            anchor_text: Set(String::new()),
            metadata: Set(None),
            discovered_at: Set(discovered_at),
            ..Default::default()
        }
        .insert(&test.db)
        .await
        .unwrap();
        store
            .replace_outbound(
                "https://example.com/a",
                vec![link("https://example.com/linked")],
            )
            .await
            .unwrap();

        let only_links_to = store
            .neighbors(
                "https://example.com/a",
                vec![RelationType::LinksTo],
                MAX_REQUESTABLE_DEPTH,
            )
            .await
            .unwrap();

        assert_eq!(urls(&only_links_to), ["https://example.com/linked"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_cycle_does_not_hang_or_crash_traversal() {
        let test = test_db::start().await;
        let store = PgEdges::new(test.db.clone());
        store
            .replace_outbound("https://example.com/a", vec![link("https://example.com/b")])
            .await
            .unwrap();
        store
            .replace_outbound("https://example.com/b", vec![link("https://example.com/a")])
            .await
            .unwrap();

        let neighbors = store
            .neighbors("https://example.com/a", vec![], MAX_REQUESTABLE_DEPTH)
            .await
            .unwrap();

        assert_eq!(urls(&neighbors), ["https://example.com/b"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_page_reachable_two_ways_appears_once_at_its_shallowest_depth() {
        let test = test_db::start().await;
        let store = PgEdges::new(test.db.clone());
        store
            .replace_outbound(
                "https://example.com/a",
                vec![link("https://example.com/b"), link("https://example.com/c")],
            )
            .await
            .unwrap();
        store
            .replace_outbound("https://example.com/c", vec![link("https://example.com/b")])
            .await
            .unwrap();

        let neighbors = store
            .neighbors("https://example.com/a", vec![], MAX_REQUESTABLE_DEPTH)
            .await
            .unwrap();

        let b_rows: Vec<&Neighbor> = neighbors
            .iter()
            .filter(|n| n.url == "https://example.com/b")
            .collect();
        assert_eq!(b_rows.len(), 1, "{neighbors:?}");
        assert_eq!(b_rows[0].depth, 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_page_with_no_outbound_links_has_no_neighbors() {
        let test = test_db::start().await;
        let store = PgEdges::new(test.db.clone());

        let neighbors = store
            .neighbors("https://example.com/lonely", vec![], MAX_REQUESTABLE_DEPTH)
            .await
            .unwrap();

        assert!(neighbors.is_empty());
    }
}
