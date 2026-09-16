// web_fetch: GET a URL, extract its readable text the same way the crawler does (common::extract
// — the second consumer that moved it there). A byte cap keeps a pathological page from being
// handed to extraction (the response body is still buffered whole by reqwest first — the cap
// bounds what gets parsed, not peak download memory), same order of magnitude as the crawler's
// own cap. SSRF guarding (`ensure_public_url`) lives here too but is deliberately NOT called by
// `fetch()` itself — the caller (`Service::run_web_fetch`, the actual untrusted-LLM-input
// boundary) applies it before calling `fetch`, mirroring where `services/crawler/src/main.rs`
// applies its own `validate_base_url` (at the RPC boundary, not inside the fetch primitive) —
// this keeps `fetch()` itself loopback-friendly for its own tests below.

use std::net::IpAddr;

use http::Uri;

const MAX_FETCH_BYTES: usize = 5 * 1024 * 1024;
const MAX_URL_CHARS: usize = 4096;

/// Rejects a URL that isn't an absolute http(s) URL resolving only to public addresses — blocks
/// SSRF via loopback/private/link-local targets (e.g. the cloud metadata endpoint
/// `169.254.169.254`, or reaching another container on this stack like `postgres:5432`), the
/// same protection `services/crawler/src/main.rs`'s `validate_base_url`/`is_private_ip` already
/// gives crawl targets.
pub async fn ensure_public_url(raw: &str) -> Result<(), String> {
    if raw.is_empty() {
        return Err("url is required".to_owned());
    }
    if raw.chars().count() > MAX_URL_CHARS {
        return Err(format!("url is longer than {MAX_URL_CHARS} characters"));
    }
    let Some(host) = raw.parse::<Uri>().ok().and_then(|uri| {
        matches!(uri.scheme_str(), Some("http" | "https"))
            .then(|| uri.host().map(str::to_owned))
            .flatten()
    }) else {
        return Err("url must be an absolute http or https URL".to_owned());
    };
    if host.is_empty() || host.eq_ignore_ascii_case("localhost") || host.ends_with(".localhost") {
        return Err(
            "url must point at a public host, not a loopback, private, or link-local address"
                .to_owned(),
        );
    }
    let literal = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = literal.parse::<IpAddr>() {
        return public_ip(ip);
    }
    let addresses = tokio::net::lookup_host((literal, 0))
        .await
        .map_err(|error| format!("url host could not be resolved: {error}"))?;
    let mut resolved_any = false;
    for address in addresses {
        resolved_any = true;
        public_ip(address.ip())?;
    }
    if !resolved_any {
        return Err("url host did not resolve to an address".to_owned());
    }
    Ok(())
}

fn public_ip(ip: IpAddr) -> Result<(), String> {
    if is_private_ip(ip) {
        Err(
            "url must point at a public host, not a loopback, private, or link-local address"
                .to_owned(),
        )
    } else {
        Ok(())
    }
}

fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            ip.is_loopback()
                || ip.is_private()
                || ip.is_link_local()
                || ip.is_unspecified()
                || ip.is_broadcast()
                || ip.is_multicast()
        }
        IpAddr::V6(ip) => {
            ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_multicast()
                || ip.is_unique_local()
                || ip.is_unicast_link_local()
                || ip
                    .to_ipv4_mapped()
                    .is_some_and(|v4| is_private_ip(IpAddr::V4(v4)))
        }
    }
}

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

    #[tokio::test]
    async fn accepts_absolute_http_and_https_urls_on_public_hosts() {
        for url in ["https://93.184.216.34", "http://93.184.216.34:8080/docs/"] {
            assert!(ensure_public_url(url).await.is_ok(), "{url}");
        }
    }

    #[tokio::test]
    async fn rejects_hosts_inside_the_network_tool_runs_in() {
        for url in [
            "http://localhost/",
            "http://app.localhost/",
            "http://127.0.0.1:8080/",
            "http://10.0.0.5/",
            "http://172.16.3.4/",
            "http://192.168.1.1/",
            "http://169.254.169.254/latest/meta-data/",
            "http://0.0.0.0/",
            "http://[::1]/",
            "http://[fd00::1]/",
            "http://[::ffff:127.0.0.1]/",
        ] {
            assert!(ensure_public_url(url).await.is_err(), "{url}");
        }
    }

    #[tokio::test]
    async fn rejects_a_hostname_that_dns_resolves_to_loopback() {
        assert!(ensure_public_url("http://localhost./").await.is_err());
    }

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
