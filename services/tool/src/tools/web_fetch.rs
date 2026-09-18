// web_fetch: GET a URL and extract its readable text, plus the SSRF guard its callers apply.

use std::net::IpAddr;
use std::time::Duration;

use common::extract::Extracted;
use http::Uri;
use serde_json::{Value, json};

const MAX_FETCH_BYTES: usize = 5 * 1024 * 1024;
const MIN_READABLE_TEXT_CHARS: usize = 40;
const ANSWERS_BEFORE_CHOOSING: usize = 2;
const MAX_URL_CHARS: usize = 4096;

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

#[must_use]
pub fn readable_page_json(page: &Extracted) -> Value {
    json!({"title": page.title, "text": page.main_text})
}

fn readable_send_failure(url: &str, error: &reqwest::Error) -> String {
    let reason = if error.is_timeout() {
        "timed out".to_owned()
    } else if error.is_connect() {
        let detail = deepest_source(error);
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

fn deepest_source(error: &reqwest::Error) -> String {
    let mut source: Option<&dyn std::error::Error> = std::error::Error::source(error);
    let mut detail = String::new();
    while let Some(current) = source {
        detail = current.to_string();
        source = current.source();
    }
    detail
}

pub async fn fetch(
    client: &reqwest::Client,
    url: &str,
    timeout: Duration,
) -> Result<Extracted, String> {
    let mut response = client
        .get(url)
        .timeout(timeout)
        .send()
        .await
        .map_err(|e| readable_send_failure(url, &e))?;
    if !response.status().is_success() {
        return Err(format!("fetch {url} returned {}", response.status()));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| readable_send_failure(url, &e))?
    {
        body.extend_from_slice(&chunk);
        if body.len() > MAX_FETCH_BYTES {
            return Err(format!(
                "{url} exceeded {MAX_FETCH_BYTES} bytes, refusing to download the rest"
            ));
        }
    }
    let page = common::extract::extract(url, &String::from_utf8_lossy(&body));
    if page.main_text.chars().count() < MIN_READABLE_TEXT_CHARS {
        return Err(format!(
            "fetch {url} extracted under {MIN_READABLE_TEXT_CHARS} characters of text, \
             so the page carried no readable content to answer from"
        ));
    }
    Ok(page)
}

pub async fn fetch_richest(
    client: &reqwest::Client,
    urls: &[String],
    timeout: Duration,
) -> Result<Extracted, String> {
    // The candidates are raced, but the winner is the fullest page rather than the fastest one:
    // whoever answers first is decided by network latency, which has nothing to do with whether the
    // page holds the answer. The race decides *when to stop waiting*; the comparison decides *what
    // to return*, and it needs something to compare against.
    let ops: Vec<(&str, crate::race::OpFn<'_, Extracted>)> = urls
        .iter()
        .map(|url| crate::race::op(url.as_str(), move || fetch(client, url, timeout)))
        .collect();
    let outcome = crate::race::race(&ops, urls.len(), ANSWERS_BEFORE_CHOOSING, timeout).await;
    let failures = outcome.failures.join("; ");
    outcome
        .taken
        .into_iter()
        .map(|(_, page)| page)
        .max_by_key(|page| page.main_text.len())
        .ok_or_else(|| format!("every url failed — {failures}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_TIMEOUT: Duration = Duration::from_secs(10);

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

    async fn serve_after(delay: Duration, html: &'static str) -> String {
        let app = axum::Router::new().route(
            "/page",
            axum::routing::get(move || async move {
                tokio::time::sleep(delay).await;
                axum::response::Html(html)
            }),
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

        let page = fetch(&client, &url, TEST_TIMEOUT).await.expect("fetch");

        assert!(page.main_text.contains("Borrowing lets code use a value"));
        assert_eq!(
            readable_page_json(&page),
            json!({"title": "Hi", "text": page.main_text})
        );
    }

    #[tokio::test]
    async fn an_endless_body_is_abandoned_once_it_passes_the_cap() {
        let app = axum::Router::new().route(
            "/endless",
            axum::routing::get(|| async {
                axum::body::Body::from_stream(futures::stream::repeat_with(|| {
                    Ok::<_, std::io::Error>(vec![b'x'; 64 * 1024])
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("addr");
        tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
        let client = reqwest::Client::new();

        let error = tokio::time::timeout(
            Duration::from_secs(30),
            fetch(&client, &format!("http://{address}/endless"), TEST_TIMEOUT),
        )
        .await
        .expect("a body that never ends must not be buffered whole")
        .unwrap_err();

        assert!(error.contains(&MAX_FETCH_BYTES.to_string()), "{error}");
    }

    #[tokio::test]
    async fn the_page_carrying_the_most_text_wins_not_the_one_that_answers_first() {
        let working = serve(
            "<html><head><title>Hi</title></head><body><article><p>Borrowing lets code use a value without taking ownership of it, which the borrow checker enforces.</p></article></body></html>",
        )
        .await;
        let dead = "http://127.0.0.1:1/nothing".to_owned();
        let client = reqwest::Client::new();

        let page = fetch_richest(&client, &[dead.clone(), working], TEST_TIMEOUT)
            .await
            .expect("one url answered");

        assert!(page.main_text.contains("Borrowing lets code use a value"));
    }

    #[tokio::test]
    async fn a_thin_page_that_answers_first_loses_to_a_fuller_one() {
        let thin = serve(
            "<html><head><title>Hi</title></head><body><article><p>This page says almost nothing at all about it.</p></article></body></html>",
        )
        .await;
        let full = serve(ARTICLE_PAGE).await;
        let client = reqwest::Client::new();

        let page = fetch_richest(&client, &[thin, full], TEST_TIMEOUT)
            .await
            .expect("both answered");

        assert!(page.main_text.contains("Borrowing lets code use a value"));
    }

    #[tokio::test]
    async fn a_third_url_does_not_hold_the_answer_back_once_two_have_replied() {
        let first = serve(ARTICLE_PAGE).await;
        let second = serve(ARTICLE_PAGE).await;
        let never = serve_after(Duration::from_secs(30), ARTICLE_PAGE).await;
        let client = reqwest::Client::new();

        let page = tokio::time::timeout(
            Duration::from_secs(5),
            fetch_richest(&client, &[first, second, never], TEST_TIMEOUT),
        )
        .await
        .expect("two answers are enough to decide")
        .expect("both answered");

        assert!(page.main_text.contains("Borrowing lets code use a value"));
    }

    #[tokio::test]
    async fn every_url_is_named_when_none_answer() {
        let client = reqwest::Client::new();

        let error = fetch_richest(
            &client,
            &[
                "http://127.0.0.1:1/a".to_owned(),
                "http://127.0.0.1:2/b".to_owned(),
            ],
            TEST_TIMEOUT,
        )
        .await
        .expect_err("both urls are dead");

        assert!(error.contains("127.0.0.1:1/a"), "{error}");
        assert!(error.contains("127.0.0.1:2/b"), "{error}");
    }

    #[tokio::test]
    async fn fetch_richest_names_every_url_that_failed() {
        let client = reqwest::Client::new();
        let urls = vec![
            "http://127.0.0.1:1/a".to_owned(),
            "http://127.0.0.1:1/b".to_owned(),
        ];
        let error = fetch_richest(&client, &urls, TEST_TIMEOUT)
            .await
            .expect_err("both urls are unreachable");
        assert!(error.contains("/a"), "{error}");
        assert!(error.contains("/b"), "{error}");
    }

    const ARTICLE_PAGE: &str = "<html><head><title>Hi</title></head><body><article><p>Borrowing lets code use a value without taking ownership of it, which the borrow checker enforces.</p></article></body></html>";
    const SCRIPT_SHELL_PAGE: &str = "<html><head><title>Conditions and Forecast</title></head><body><div id=\"root\"></div><script src=\"/app.js\"></script></body></html>";

    #[tokio::test]
    async fn a_page_that_extracts_no_usable_text_fails_and_names_the_url() {
        let url = serve(SCRIPT_SHELL_PAGE).await;
        let client = reqwest::Client::new();

        let error = fetch(&client, &url, TEST_TIMEOUT)
            .await
            .expect_err("a page whose body is rendered by script carries no text");

        assert!(error.contains(&url), "{error}");
        assert!(
            error.contains(&MIN_READABLE_TEXT_CHARS.to_string()),
            "{error}"
        );
    }

    #[tokio::test]
    async fn a_page_that_extracted_nothing_usable_is_passed_over() {
        let empty = serve(SCRIPT_SHELL_PAGE).await;
        let readable = serve(ARTICLE_PAGE).await;
        let client = reqwest::Client::new();

        let page = fetch_richest(&client, &[empty, readable], TEST_TIMEOUT)
            .await
            .expect("one url carried real text");

        assert!(page.main_text.contains("Borrowing lets code use a value"));
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

        let error = fetch(&client, &format!("http://{address}/missing"), TEST_TIMEOUT)
            .await
            .unwrap_err();

        assert!(error.contains("404"));
    }

    #[tokio::test]
    async fn a_transport_failure_says_what_actually_went_wrong() {
        let client = reqwest::Client::new();

        let unused_port = "http://127.0.0.1:1/page";

        let error = fetch(&client, unused_port, TEST_TIMEOUT).await.unwrap_err();

        assert!(error.contains(unused_port), "{error}");
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
        let client = reqwest::Client::new();

        let error = fetch(
            &client,
            &format!("http://{address}/slow"),
            Duration::from_millis(50),
        )
        .await
        .unwrap_err();

        assert!(error.contains("timed out"), "{error}");
    }
}
