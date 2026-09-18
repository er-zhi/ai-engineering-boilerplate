// web_search backed by the You.com Search API, behind SearchProvider.

use serde::Deserialize;
use url::Url;

use crate::providers::{SearchProvider, SearchResult};

const SEARCH_URL: &str = "https://ydc-index.io/v1/search";

pub struct YouSearchProvider {
    api_key: String,
    base_url: String,
    client: reqwest::Client,
}

impl YouSearchProvider {
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
struct YouResponse {
    results: Option<YouResults>,
}
#[derive(Deserialize)]
struct YouResults {
    web: Vec<YouResult>,
}
#[derive(Deserialize)]
struct YouResult {
    title: String,
    url: String,
    description: String,
}

impl SearchProvider for YouSearchProvider {
    async fn search(&self, query: &str, limit: u8) -> Result<Vec<SearchResult>, String> {
        let mut url = Url::parse(&self.base_url).map_err(|e| e.to_string())?;
        url.query_pairs_mut()
            .append_pair("query", query)
            .append_pair("count", &limit.to_string());
        let response = self
            .client
            .get(url.as_str())
            .header("X-API-Key", &self.api_key)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !response.status().is_success() {
            return Err(format!("you.com search returned {}", response.status()));
        }
        let parsed: YouResponse = response.json().await.map_err(|e| e.to_string())?;
        let mut results: Vec<SearchResult> = parsed
            .results
            .map(|results| results.web)
            .unwrap_or_default()
            .into_iter()
            .map(|r| SearchResult {
                title: r.title,
                url: r.url,
                snippet: r.description,
                text: None,
            })
            .collect();
        enforce_limit_client_side(&mut results, limit);
        Ok(results)
    }
}

fn enforce_limit_client_side(results: &mut Vec<SearchResult>, limit: u8) {
    results.truncate(limit as usize);
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::Query;
    use axum::response::Json;
    use std::collections::HashMap;

    async fn fake_you(Query(params): Query<HashMap<String, String>>) -> Json<serde_json::Value> {
        assert_eq!(
            params.get("query").map(String::as_str),
            Some("rust async traits")
        );
        Json(serde_json::json!({
            "results": {"web": [{"title": "Async traits", "url": "https://example.com/a", "description": "A guide."}]}
        }))
    }

    async fn serve() -> String {
        let app = axum::Router::new().route("/v1/search", axum::routing::get(fake_you));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("addr");
        tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
        format!("http://{address}/v1/search")
    }

    #[tokio::test]
    async fn search_parses_you_results() {
        let url = serve().await;
        let provider = YouSearchProvider::with_base_url("fake-key".to_owned(), url);

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
                text: None,
            }]
        );
    }

    #[tokio::test]
    async fn search_truncates_to_limit() {
        async fn fake_you_many() -> Json<serde_json::Value> {
            Json(serde_json::json!({
                "results": {"web": [
                    {"title": "One", "url": "https://example.com/1", "description": "d"},
                    {"title": "Two", "url": "https://example.com/2", "description": "d"},
                    {"title": "Three", "url": "https://example.com/3", "description": "d"},
                ]}
            }))
        }
        let app = axum::Router::new().route("/v1/search", axum::routing::get(fake_you_many));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("addr");
        tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
        let url = format!("http://{address}/v1/search");
        let provider = YouSearchProvider::with_base_url("fake-key".to_owned(), url);

        let results = provider.search("q", 2).await.expect("search");

        assert_eq!(results.len(), 2);
    }
}
