// web_search prefetch: after a search returns, race fetches of its top hits so the agent can
// answer straight from the page instead of spending a further generative round asking for the
// same URLs back by name. See docs/superpowers/plans/2026-09-18-latency-round-two.md, Task C.

use std::collections::HashMap;
use std::time::Duration;

use crate::providers::SearchResult;
use crate::race;
use crate::tools::web_fetch;

/// How many of a search's top hits are worth racing a fetch for. Four candidates raced down to
/// `PREFETCH_TAKE` successes survives a slow or dead host without paying for a fifth attempt once
/// two answers are already in.
const PREFETCH_CANDIDATES: usize = 4;
const PREFETCH_TAKE: usize = 2;
/// Per-operation cap. Measured fetch p50 in this deployment is 781 ms
/// (`docs/superpowers/plans/2026-09-18-latency-round-two.md`); 1 s gives headroom without letting
/// one slow host hold the search's own return back by much more than that measured cost.
const PREFETCH_TIMEOUT: Duration = Duration::from_secs(1);

/// After a search returns, races fetches of its top candidates and attaches the readable text of
/// whichever succeed. Every candidate is checked with `ensure_public_url` before any request goes
/// out, exactly as `run_web_fetch` checks a caller's urls — these came back from a third-party
/// search provider and are just as untrusted.
///
/// A hit that is not among the top candidates, fails that check, or loses the race keeps its
/// snippet and gains nothing: this function only ever *adds* a `text` field, so a slow, dead, or
/// hostile page degrades to exactly today's output. It cannot itself fail — there is nothing here
/// for a caller to propagate as an error.
pub async fn attach_prefetched_text(client: &reqwest::Client, results: &mut [SearchResult]) {
    let mut candidates = Vec::new();
    for hit in results.iter().take(PREFETCH_CANDIDATES) {
        if web_fetch::ensure_public_url(&hit.url).await.is_ok() {
            candidates.push(hit.url.clone());
        }
    }
    let fetched = race_fetch(client, &candidates, PREFETCH_TAKE, PREFETCH_TIMEOUT).await;
    for hit in results.iter_mut() {
        if let Some(text) = fetched.get(&hit.url) {
            hit.text = Some(text.clone());
        }
    }
}

/// The racing mechanism on its own, with no SSRF guard applied — kept separate so it can be
/// exercised directly against a loopback server the same way `web_fetch::fetch_richest`'s own
/// tests do. `attach_prefetched_text` is what applies the guard before any URL ever reaches this.
async fn race_fetch(
    client: &reqwest::Client,
    urls: &[String],
    take: usize,
    per_op_timeout: Duration,
) -> HashMap<String, String> {
    if urls.is_empty() {
        return HashMap::new();
    }
    let ops: Vec<(&str, race::OpFn<'_, common::extract::Extracted>)> = urls
        .iter()
        .map(|url| {
            race::op(url.as_str(), move || {
                web_fetch::fetch(client, url, per_op_timeout)
            })
        })
        .collect();
    let outcome = race::race(&ops, ops.len(), take, per_op_timeout).await;
    outcome
        .taken
        .into_iter()
        .map(|(url, page)| (url.to_owned(), page.main_text))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    const TEST_TIMEOUT: Duration = Duration::from_secs(1);

    fn client() -> reqwest::Client {
        crate::service::bounded_http_client().expect("build the guarded client")
    }

    fn hit(url: &str) -> SearchResult {
        SearchResult {
            title: "Title".to_owned(),
            url: url.to_owned(),
            snippet: "Snippet".to_owned(),
            text: None,
        }
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

    const ARTICLE_PAGE: &str = "<html><head><title>Hi</title></head><body><article><p>Borrowing lets code use a value without taking ownership of it, which the borrow checker enforces.</p></article></body></html>";

    // race_fetch has no guard of its own, so a dead loopback port is a legitimate "fetch fails"
    // case for it (unlike attach_prefetched_text, which would refuse the address before ever
    // trying) — the same trick web_fetch.rs's own fetch_richest tests use.
    #[tokio::test]
    async fn race_fetch_skips_a_url_whose_fetch_fails() {
        let dead = "http://127.0.0.1:1/nothing".to_owned();
        let working = serve(ARTICLE_PAGE).await;

        let fetched =
            race_fetch(&client(), &[dead.clone(), working.clone()], 2, TEST_TIMEOUT).await;

        assert!(!fetched.contains_key(&dead), "{fetched:?}");
        assert!(
            fetched
                .get(&working)
                .is_some_and(|text| text.contains("Borrowing lets code use a value")),
            "{fetched:?}"
        );
    }

    #[tokio::test]
    async fn race_fetch_attaches_the_text_of_two_successes() {
        let first = serve(ARTICLE_PAGE).await;
        let second = serve(ARTICLE_PAGE).await;

        let fetched =
            race_fetch(&client(), &[first.clone(), second.clone()], 2, TEST_TIMEOUT).await;

        assert_eq!(fetched.len(), 2);
        assert!(
            fetched
                .values()
                .all(|text| text.contains("Borrowing lets code use a value"))
        );
    }

    #[tokio::test]
    async fn race_fetch_stops_at_the_quorum_without_waiting_for_a_third() {
        let first = serve(ARTICLE_PAGE).await;
        let second = serve(ARTICLE_PAGE).await;
        let never = "http://127.0.0.1:1/never".to_owned();

        let fetched = tokio::time::timeout(
            Duration::from_secs(5),
            race_fetch(&client(), &[first, second, never], 2, TEST_TIMEOUT),
        )
        .await
        .expect("two successes are enough to stop");

        assert_eq!(fetched.len(), 2);
    }

    // Required test: "a hit whose fetch fails renders exactly as today." The hit's url passes
    // ensure_public_url (a routable, public-classified address per RFC 5737 — see
    // web_fetch.rs's own `a_redirect_to_a_public_address_is_followed`, which leans on the same
    // range for the same reason: this sandbox has no route to the real internet, so the request
    // reliably fails at the network layer, never at the guard) but the fetch itself fails, so the
    // hit must come back with no `text` field at all — the exact shape a plain search returns.
    #[tokio::test]
    async fn a_hit_whose_fetch_fails_renders_exactly_as_today() {
        let mut results = vec![hit("http://203.0.113.5/")];

        attach_prefetched_text(&client(), &mut results).await;

        assert_eq!(results[0].text, None);
        assert_eq!(
            serde_json::to_value(&results[0]).expect("serializes"),
            serde_json::json!({"title": "Title", "url": "http://203.0.113.5/", "snippet": "Snippet"}),
            "a fetch failure must not add a text key at all"
        );
    }

    // Required test: a search whose every prefetch fails (here: every candidate is a private
    // address, so none is ever attempted) returns the snippet-only result — today's output — and
    // attach_prefetched_text has no error path to report through in the first place.
    #[tokio::test]
    async fn a_search_whose_every_prefetch_fails_returns_the_snippet_only_result() {
        let loopback_one = serve(ARTICLE_PAGE).await;
        let loopback_two = serve(ARTICLE_PAGE).await;
        let before = vec![hit(&loopback_one), hit(&loopback_two)];
        let mut results = before.clone();

        attach_prefetched_text(&client(), &mut results).await;

        assert_eq!(
            results, before,
            "a loopback address must be refused by the guard, so nothing changes"
        );
    }

    // Required test: a prefetch candidate on a private address is refused before any request goes
    // out — proven the same way web_fetch.rs proves it for a redirect hop: a real listener that
    // counts hits, never touched.
    #[tokio::test]
    async fn a_prefetch_candidate_on_a_private_address_is_refused_before_any_request_goes_out() {
        let hits = Arc::new(AtomicUsize::new(0));
        let counted = hits.clone();
        let app = axum::Router::new().route(
            "/secret",
            axum::routing::get(move || {
                let counted = counted.clone();
                async move {
                    counted.fetch_add(1, Ordering::SeqCst);
                    "leaked"
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("addr");
        tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
        let mut results = vec![hit(&format!("http://{address}/secret"))];

        attach_prefetched_text(&client(), &mut results).await;

        assert_eq!(
            hits.load(Ordering::SeqCst),
            0,
            "a private address must never be requested"
        );
        assert_eq!(results[0].text, None);
    }

    #[tokio::test]
    async fn attach_prefetched_text_only_considers_the_top_candidates() {
        // A fifth hit beyond PREFETCH_CANDIDATES must never be raced, even though it would
        // otherwise succeed — proven by using a real server for it and checking it never gains a
        // text field, while the local guard rejects the first four (private addresses) as usual.
        let mut results: Vec<SearchResult> = (0..PREFETCH_CANDIDATES)
            .map(|index| hit(&format!("http://127.0.0.1:1/candidate-{index}")))
            .collect();
        let fifth = serve(ARTICLE_PAGE).await;
        results.push(hit(&fifth));

        attach_prefetched_text(&client(), &mut results).await;

        assert_eq!(
            results[PREFETCH_CANDIDATES].text, None,
            "a hit beyond the candidate window must not be fetched"
        );
    }
}
