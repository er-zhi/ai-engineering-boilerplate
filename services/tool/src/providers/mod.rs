pub mod brave;
pub mod you;

use brave::{BraveSearchProvider, SearchProvider, SearchResult};
use you::YouSearchProvider;

/// The web_search provider chosen at startup (main.rs), based on which API key is set —
/// You.com preferred, then Brave, else `Unconfigured` so Execute returns a clear error instead
/// of silently calling Brave with an empty key.
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
    /// Picks You.com over Brave when both keys are set (env var wins over env var in place,
    /// not merged), and logs the choice — never the key itself.
    #[must_use]
    pub fn from_env(you_api_key: String, brave_api_key: String) -> Self {
        let (name, provider) = if !you_api_key.is_empty() {
            ("you.com", Self::You(YouSearchProvider::new(you_api_key)))
        } else if !brave_api_key.is_empty() {
            (
                "brave",
                Self::Brave(BraveSearchProvider::new(brave_api_key)),
            )
        } else {
            ("none configured", Self::Unconfigured)
        };
        tracing::info!("web search provider: {name}");
        provider
    }
}
