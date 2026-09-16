// web_search: Brave Search API behind SearchProvider, so a future provider swap doesn't touch
// the tool that calls it. BRAVE_SEARCH_API_KEY lives directly in this service's env today —
// documented as temporary, moves to Integrations Service once that exists (spec, "Что это").

use serde::Deserialize;
use url::Url;

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

pub trait SearchProvider: Send + Sync {
    fn search(
        &self,
        query: &str,
        limit: u8,
    ) -> impl Future<Output = Result<Vec<SearchResult>, String>> + Send;
}

pub struct BraveSearchProvider {
    api_key: String,
    base_url: String,
    client: reqwest::Client,
}

impl BraveSearchProvider {
    #[must_use]
    pub fn new(api_key: String) -> Self {
        Self {
            api_key,
            base_url: "https://api.search.brave.com/res/v1/web/search".to_owned(),
            client: reqwest::Client::new(),
        }
    }

    #[must_use]
    #[cfg(test)]
    fn with_base_url(api_key: String, base_url: String) -> Self {
        Self {
            api_key,
            base_url,
            client: reqwest::Client::new(),
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
