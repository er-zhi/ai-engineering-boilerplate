// The vendor-neutral web-search port; the vendor modules under it only implement it.

pub mod brave;
pub mod you;

use brave::BraveSearchProvider;
use you::YouSearchProvider;

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
    /// The hit's readable text, when a prefetch (`tools::web_search::attach_prefetched_text`)
    /// raced a fetch of it and won. Absent whenever prefetching was not attempted or did not
    /// succeed for this hit — `None` serializes to no `text` key at all, so a hit that gained
    /// nothing renders exactly as a search result always has.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

pub trait SearchProvider: Send + Sync {
    fn search(
        &self,
        query: &str,
        limit: u8,
    ) -> impl Future<Output = Result<Vec<SearchResult>, String>> + Send;
}

pub enum AnySearchProvider {
    You(YouSearchProvider),
    Brave(BraveSearchProvider),
    Unconfigured,
}

impl SearchProvider for AnySearchProvider {
    async fn search(&self, query: &str, limit: u8) -> Result<Vec<SearchResult>, String> {
        match self {
            Self::You(provider) => provider.search(query, limit).await,
            Self::Brave(provider) => provider.search(query, limit).await,
            Self::Unconfigured => Err(
                "web search is not configured: set YOU_SEARCH_API_KEY or BRAVE_SEARCH_API_KEY"
                    .to_owned(),
            ),
        }
    }
}

impl AnySearchProvider {
    #[must_use]
    pub fn from_api_keys(
        you_api_key: String,
        brave_api_key: String,
        bounded_client: reqwest::Client,
    ) -> Self {
        let (name, provider) = if !you_api_key.is_empty() {
            (
                "you.com",
                Self::You(YouSearchProvider::new(you_api_key, bounded_client)),
            )
        } else if !brave_api_key.is_empty() {
            (
                "brave",
                Self::Brave(BraveSearchProvider::new(brave_api_key, bounded_client)),
            )
        } else {
            ("none configured", Self::Unconfigured)
        };
        tracing::info!("web search provider: {name}");
        provider
    }
}
