// Test-only website on 127.0.0.1 that records every path the crawler requests.

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
    pub async fn start<P, L>(pages: impl IntoIterator<Item = (P, L)>) -> Self
    where
        P: Into<String>,
        L: Into<String>,
    {
        Self::start_with_response_delay(pages, Duration::ZERO).await
    }

    pub async fn start_with_html<P, B>(pages: impl IntoIterator<Item = (P, B)>) -> Self
    where
        P: Into<String>,
        B: Into<String>,
    {
        let pages = pages
            .into_iter()
            .map(|(path, body)| (path.into(), body.into()))
            .collect();
        Self::start_pages(pages, Duration::ZERO).await
    }

    pub async fn start_with_response_delay<P, L>(
        pages: impl IntoIterator<Item = (P, L)>,
        response_delay: Duration,
    ) -> Self
    where
        P: Into<String>,
        L: Into<String>,
    {
        let pages = pages
            .into_iter()
            .map(|(path, hrefs)| (path.into(), page_linking_to(&hrefs.into())))
            .collect();
        Self::start_pages(pages, response_delay).await
    }

    async fn start_pages(pages: HashMap<String, String>, response_delay: Duration) -> Self {
        let pages = Arc::new(pages);
        let requested = Arc::new(Mutex::new(Vec::new()));

        let app = axum::Router::new().fallback({
            let requested = requested.clone();
            move |uri: Uri| {
                let pages = pages.clone();
                let requested = requested.clone();
                async move {
                    requested.lock().unwrap().push(uri.path().to_owned());
                    tokio::time::sleep(response_delay).await;
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

    pub fn requested_pages(&self) -> Vec<String> {
        self.requested
            .lock()
            .unwrap()
            .iter()
            .filter(|path| path.as_str() != "/robots.txt")
            .cloned()
            .collect()
    }
}

pub async fn unreachable_url() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    format!("http://{address}/")
}

fn page_linking_to(space_separated_hrefs: &str) -> String {
    let anchors: String = space_separated_hrefs
        .split_whitespace()
        .map(|href| format!(r#"<a href="{href}">{href}</a>"#))
        .collect();
    format!("<!doctype html><html><body>{anchors}</body></html>")
}
