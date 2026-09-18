// web_search backed by the Brave Search API, behind SearchProvider.

use serde::Deserialize;
use url::Url;

use crate::providers::{SearchProvider, SearchResult};

const SEARCH_URL: &str = "https://api.search.brave.com/res/v1/web/search";

pub struct BraveSearchProvider {
    api_key: String,
    base_url: String,
    client: reqwest::Client,
}

impl BraveSearchProvider {
    #[must_use]
    pub fn new(api_key: String, client: reqwest::Client) -> Self {
        Self {
            api_key,
            base_url: SEARCH_URL.to_owned(),
            client,
        }
    }

    #[must_use]
    #[cfg(test)]
    fn with_base_url(api_key: String, base_url: String) -> Self {
        Self {
            api_key,
            base_url,
            client: crate::service::redirect_following_http_client().expect("client"),
        }
    }
}

#[derive(Deserialize)]
struct BraveResponse {
    web: Option<BraveWeb>,
}
#[derive(Deserialize)]
struct BraveWeb {
    results: Vec<BraveResult>,
}
#[derive(Deserialize)]
struct BraveResult {
    title: String,
    url: String,
    description: String,
}

impl SearchProvider for BraveSearchProvider {
    async fn search(&self, query: &str, limit: u8) -> Result<Vec<SearchResult>, String> {
        let mut url = Url::parse(&self.base_url).map_err(|e| e.to_string())?;
        url.query_pairs_mut()
            .append_pair("q", query)
            .append_pair("count", &limit.to_string());
        let response = self
            .client
            .get(url.as_str())
            .header("X-Subscription-Token", &self.api_key)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !response.status().is_success() {
            return Err(format!("brave search returned {}", response.status()));
        }
        let parsed: BraveResponse = response.json().await.map_err(|e| e.to_string())?;
        Ok(parsed
            .web
            .map(|web| web.results)
            .unwrap_or_default()
            .into_iter()
            .map(|r| SearchResult {
                title: r.title,
                url: r.url,
                snippet: r.description,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::Query;
    use axum::response::Json;
    use std::collections::HashMap;

    async fn fake_brave(Query(params): Query<HashMap<String, String>>) -> Json<serde_json::Value> {
        assert_eq!(
            params.get("q").map(String::as_str),
            Some("rust async traits")
        );
        Json(serde_json::json!({
            "web": {"results": [{"title": "Async traits", "url": "https://example.com/a", "description": "A guide."}]}
        }))
    }

    async fn serve() -> String {
        let app = axum::Router::new().route("/res/v1/web/search", axum::routing::get(fake_brave));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("addr");
        tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
        format!("http://{address}/res/v1/web/search")
    }

    async fn redirect_301(location: &'static str) -> impl axum::response::IntoResponse {
        (
            axum::http::StatusCode::MOVED_PERMANENTLY,
            [(axum::http::header::LOCATION, location)],
        )
    }

    async fn fake_brave_unconditional() -> Json<serde_json::Value> {
        Json(serde_json::json!({
            "web": {"results": [{"title": "Async traits", "url": "https://example.com/a", "description": "A guide."}]}
        }))
    }

    // Proves the provider's client follows a vendor's own 301 rather than failing on it: a search
    // endpoint answering an http→https upgrade, a moved path, or a CDN hop must still yield results,
    // not `"brave search returned 301 Moved Permanently"`.
    async fn serve_via_301_redirect() -> String {
        let app = axum::Router::new()
            .route(
                "/old",
                axum::routing::get(|| redirect_301("/res/v1/web/search")),
            )
            .route(
                "/res/v1/web/search",
                axum::routing::get(fake_brave_unconditional),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("addr");
        tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
        format!("http://{address}/old")
    }

    #[tokio::test]
    async fn a_301_redirect_to_the_real_endpoint_is_followed() {
        let url = serve_via_301_redirect().await;
        let provider = BraveSearchProvider::with_base_url("fake-key".to_owned(), url);

        let results = provider
            .search("rust async traits", 5)
            .await
            .expect("a 301 hop should be followed, not treated as a failed request");

        assert_eq!(
            results,
            vec![SearchResult {
                title: "Async traits".to_owned(),
                url: "https://example.com/a".to_owned(),
                snippet: "A guide.".to_owned(),
            }]
        );
    }

    #[tokio::test]
    async fn search_parses_brave_results() {
        let url = serve().await;
        let provider = BraveSearchProvider::with_base_url("fake-key".to_owned(), url);

        let results = provider
            .search("rust async traits", 5)
            .await
            .expect("search");

        assert_eq!(
            results,
            vec![SearchResult {
                title: "Async traits".to_owned(),
                url: "https://example.com/a".to_owned(),
                snippet: "A guide.".to_owned(),
            }]
        );
    }
}
