// web_fetch: GET a URL, extract its readable text the same way the crawler does (common::extract
// — the second consumer that moved it there). A byte cap keeps a pathological page from being
// handed to extraction (the response body is still buffered whole by reqwest first — the cap
// bounds what gets parsed, not peak download memory), same order of magnitude as the crawler's
// own cap.

const MAX_FETCH_BYTES: usize = 5 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct FetchedPage {
    pub title: String,
    pub text: String,
}

pub async fn fetch(client: &reqwest::Client, url: &str) -> Result<FetchedPage, String> {
    let response = client.get(url).send().await.map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        return Err(format!("fetch {url} returned {}", response.status()));
    }
    let bytes = response.bytes().await.map_err(|e| e.to_string())?;
    if bytes.len() > MAX_FETCH_BYTES {
        return Err(format!(
            "{url} exceeded {MAX_FETCH_BYTES} bytes, refusing to parse"
        ));
    }
    let html = String::from_utf8_lossy(&bytes);
    let extracted = common::extract::extract(url, &html);
    Ok(FetchedPage {
        title: extracted.title,
        text: extracted.main_text,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn serve(html: &'static str) -> String {
        let app = axum::Router::new().route(
            "/page",
            axum::routing::get(move || async move { axum::response::Html(html) }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("addr");
        tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
        format!("http://{address}/page")
    }

    #[tokio::test]
    async fn fetch_extracts_title_and_text() {
        let url = serve(
            "<html><head><title>Hi</title></head><body><article><h1>Hi</h1><p>Borrowing lets code use a value without taking ownership of it. References must always be valid, which the borrow checker enforces at compile time.</p></article></body></html>",
        )
        .await;
        let client = reqwest::Client::new();

        let page = fetch(&client, &url).await.expect("fetch");

        assert!(page.text.contains("Borrowing lets code use a value"));
    }

    #[tokio::test]
    async fn a_non_success_status_is_an_error() {
        let app = axum::Router::new().route(
            "/missing",
            axum::routing::get(|| async { axum::http::StatusCode::NOT_FOUND }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("addr");
        tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
        let client = reqwest::Client::new();

        let error = fetch(&client, &format!("http://{address}/missing"))
            .await
            .unwrap_err();

        assert!(error.contains("404"));
    }
}
