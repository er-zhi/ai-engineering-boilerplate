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

use futures::StreamExt;
use futures::stream::FuturesUnordered;
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

/// Why a `reqwest` send failed, in words an LLM can act on. reqwest's own Display for a transport
/// failure is the near-useless "error sending request for url (...)" whatever went wrong, so an
/// agent reading the error back as an observation can't tell "try again" from "this host will
/// never answer me, use web_search instead".
fn send_error(url: &str, error: &reqwest::Error) -> String {
    let reason = if error.is_timeout() {
        "timed out".to_owned()
    } else if error.is_connect() {
        // DNS failure, refused connection and TLS failure all surface as connect errors; the
        // source chain is the only thing that separates them.
        let mut source: Option<&dyn std::error::Error> = std::error::Error::source(error);
        let mut detail = String::new();
        while let Some(current) = source {
            detail = current.to_string();
            source = current.source();
        }
        let lowered = detail.to_lowercase();
        if lowered.contains("dns") || lowered.contains("name or service not known") {
            format!("host could not be resolved ({detail})")
        } else if lowered.contains("refused") {
            format!("connection refused ({detail})")
        } else if detail.is_empty() {
            "could not connect".to_owned()
        } else {
            format!("could not connect ({detail})")
        }
    } else if error.is_redirect() {
        "too many redirects".to_owned()
    } else if error.is_body() || error.is_decode() {
        "the response body could not be read".to_owned()
    } else {
        format!("the request failed ({error})")
    };
    format!("fetch {url} failed: {reason}")
}

pub async fn fetch(client: &reqwest::Client, url: &str) -> Result<FetchedPage, String> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| send_error(url, &e))?;
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

/// Fetches every URL concurrently and returns the first page that comes back, dropping the rest.
/// Public pages carrying the same fact are individually unreliable — one 403s a datacentre IP,
/// another answers with a JavaScript shell, a third is just slow — so an agent handed a single
/// URL spends a whole turn per failure. Racing the candidates it already has costs one turn
/// whatever any single source does. All of them failing is one error listing every reason, so the
/// model can tell "try other pages" from "this question needs a different search".
pub async fn fetch_first(client: &reqwest::Client, urls: &[String]) -> Result<FetchedPage, String> {
    let mut running: FuturesUnordered<_> = urls
        .iter()
        .map(|url| async move { (url, fetch(client, url).await) })
        .collect();
    let mut errors = Vec::new();
    while let Some((url, result)) = running.next().await {
        match result {
            Ok(page) => return Ok(page),
            Err(error) => errors.push(format!("{url}: {error}")),
        }
    }
    Err(format!("every url failed — {}", errors.join("; ")))
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

    /// The point of racing: the caller hands over candidates without knowing which host will
    /// actually answer, and a dead one costs latency rather than the answer.
    #[tokio::test]
    async fn fetch_first_returns_the_page_from_whichever_url_answers() {
        let working = serve(
            "<html><head><title>Hi</title></head><body><article><p>Borrowing lets code use a value without taking ownership of it, which the borrow checker enforces.</p></article></body></html>",
        )
        .await;
        let dead = "http://127.0.0.1:1/nothing".to_owned();
        let client = reqwest::Client::new();

        let page = fetch_first(&client, &[dead.clone(), working])
            .await
            .expect("one url answered");

        assert!(page.text.contains("Borrowing lets code use a value"));
    }

    #[tokio::test]
    async fn fetch_first_reports_every_url_when_none_answer() {
        let client = reqwest::Client::new();

        let error = fetch_first(
            &client,
            &[
                "http://127.0.0.1:1/a".to_owned(),
                "http://127.0.0.1:2/b".to_owned(),
            ],
        )
        .await
        .expect_err("both urls are dead");

        assert!(error.contains("127.0.0.1:1/a"), "{error}");
        assert!(error.contains("127.0.0.1:2/b"), "{error}");
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

    #[tokio::test]
    async fn a_transport_failure_says_what_actually_went_wrong() {
        let client = reqwest::Client::new();

        // Nothing listens on port 1 — a connect failure, not reqwest's generic "error sending
        // request for url".
        let error = fetch(&client, "http://127.0.0.1:1/page").await.unwrap_err();
        assert!(error.contains("http://127.0.0.1:1/page"), "{error}");
        assert!(
            error.contains("connect") || error.contains("refused"),
            "{error}"
        );
        assert!(!error.starts_with("error sending request"), "{error}");
    }

    #[tokio::test]
    async fn a_timeout_is_reported_as_a_timeout() {
        let app = axum::Router::new().route(
            "/slow",
            axum::routing::get(|| async {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                "never"
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("addr");
        tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(50))
            .build()
            .expect("client");

        let error = fetch(&client, &format!("http://{address}/slow"))
            .await
            .unwrap_err();

        assert!(error.contains("timed out"), "{error}");
    }
}
