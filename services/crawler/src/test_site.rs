// Test-only website on 127.0.0.1, so crawl tests never touch the internet.
// It records every path requested, so tests can assert what the crawler actually fetched.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::{StatusCode, Uri};
use axum::response::{Html, IntoResponse};

pub struct TestSite {
    base_url: String,
    requested: Arc<Mutex<Vec<String>>>,
}

impl TestSite {
    /// Serves one HTML page per `(path, links)` pair; `links` is a space-separated list of hrefs.
    /// Any other path answers 404.
    pub async fn start<P, L>(pages: impl IntoIterator<Item = (P, L)>) -> Self
    where
        P: Into<String>,
        L: Into<String>,
    {
        Self::start_with_delay(pages, Duration::ZERO).await
    }

    /// Like `start`, but every response waits `delay` first, so a crawl stays in progress long enough to observe.
    pub async fn start_with_delay<P, L>(
        pages: impl IntoIterator<Item = (P, L)>,
        delay: Duration,
    ) -> Self
    where
        P: Into<String>,
        L: Into<String>,
    {
        let pages: Arc<HashMap<String, String>> = Arc::new(
            pages
                .into_iter()
                .map(|(path, links)| (path.into(), page_linking_to(&links.into())))
                .collect(),
        );
        let requested = Arc::new(Mutex::new(Vec::new()));

        let app = axum::Router::new().fallback({
            let requested = requested.clone();
            move |uri: Uri| {
                let pages = pages.clone();
                let requested = requested.clone();
                async move {
                    requested.lock().unwrap().push(uri.path().to_owned());
                    tokio::time::sleep(delay).await;
                    match pages.get(uri.path()) {
                        Some(body) => Html(body.clone()).into_response(),
                        None => StatusCode::NOT_FOUND.into_response(),
                    }
                }
            }
        });

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        Self {
            base_url,
            requested,
        }
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    /// Page paths requested so far, not counting robots.txt.
    pub fn requested(&self) -> Vec<String> {
        self.requested
            .lock()
            .unwrap()
            .iter()
            .filter(|path| path.as_str() != "/robots.txt")
            .cloned()
            .collect()
    }
}

/// A URL nothing listens on: bind a free port, then release it.
pub async fn unreachable_url() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    format!("http://{address}/")
}

fn page_linking_to(links: &str) -> String {
    let anchors: String = links
        .split_whitespace()
        .map(|href| format!(r#"<a href="{href}">{href}</a>"#))
        .collect();
    format!("<!doctype html><html><body>{anchors}</body></html>")
}
