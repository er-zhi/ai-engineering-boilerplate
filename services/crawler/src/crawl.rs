// Runs one crawl with spider, reporting in-scope pages and every fetched page separately.

use std::time::Duration;

use spider::compact_str::CompactString;
use spider::page::Page;
use spider::website::Website;
use tokio::sync::broadcast::Receiver;
use tokio::sync::broadcast::error::{RecvError, TryRecvError};

use crate::scope::Scope;

const USER_AGENT: &str = "ai-engineering-boilerplate-crawler/0.1";

#[derive(Clone, Copy)]
pub struct Limits {
    pub max_pages: u32,
    pub request_timeout: Duration,
}

#[derive(Debug, PartialEq)]
pub struct Unreachable;

pub struct CountedPage {
    pub url: String,
    pub html: String,
    pub status: u16,
}

pub struct FetchedPage {
    pub final_url: String,
    pub html: String,
}

pub async fn crawl(
    base_url: &str,
    scope: &Scope,
    limits: Limits,
    mut on_counted: impl FnMut(CountedPage) + Send,
    mut on_fetched: impl FnMut(FetchedPage) + Send,
) -> Result<(), Unreachable> {
    let blacklist: Vec<CompactString> = scope
        .blacklist()
        .into_iter()
        .map(CompactString::from)
        .collect();

    let mut website = Website::new(base_url);
    website
        .with_respect_robots_txt(true)
        .with_subdomains(false)
        .with_limit(limits.max_pages)
        .with_request_timeout(Some(limits.request_timeout))
        .with_user_agent(Some(USER_AGENT))
        .with_blacklist_url(Some(blacklist));
    let mut pages = website.subscribe(0);

    let mut any_fetched = false;
    let mut count_fetched_page = |page: Page| {
        if page.status_code.is_success() {
            any_fetched = true;
            let html = page.get_html();
            if html.len() > crate::links::MAX_PARSEABLE_HTML_BYTES {
                return;
            }
            let counts_toward_scope = scope.counts(page.get_url());
            if counts_toward_scope {
                on_counted(CountedPage {
                    url: page.get_url().to_owned(),
                    html: html.clone(),
                    status: page.status_code.as_u16(),
                });
            }
            let final_url = page.get_url_final().to_owned();
            if final_url.len() <= crate::links::MAX_URL_BYTES {
                on_fetched(FetchedPage { final_url, html });
            }
        }
    };

    let crawling = website.crawl();
    tokio::pin!(crawling);
    loop {
        tokio::select! {
            () = &mut crawling => break,
            received = pages.recv() => match received {
                Ok(page) => count_fetched_page(page),
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => {
                    crawling.as_mut().await;
                    break;
                }
            },
        }
    }
    drain_pages_sent_before_the_crawl_returned(&mut pages, &mut count_fetched_page);

    if any_fetched {
        Ok(())
    } else {
        Err(Unreachable)
    }
}

fn drain_pages_sent_before_the_crawl_returned(
    pages: &mut Receiver<Page>,
    mut count_fetched_page: impl FnMut(Page),
) {
    loop {
        match pages.try_recv() {
            Ok(page) => count_fetched_page(page),
            Err(TryRecvError::Lagged(_)) => {}
            Err(TryRecvError::Empty | TryRecvError::Closed) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use common::proto::crawler::v1::CrawlScope;

    use super::*;
    use crate::test_site::{TestSite, unreachable_url};

    const LIMITS: Limits = Limits {
        max_pages: 50,
        request_timeout: Duration::from_secs(5),
    };

    fn everything() -> Scope {
        Scope::new(&CrawlScope::default())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn counts_in_scope_pages_reached_through_out_of_scope_pages() {
        let site = TestSite::start([
            ("/", "/docs/a /blog/x /admin/secret"),
            ("/docs/a", "/"),
            ("/blog/x", "/docs/b"),
            ("/docs/b", ""),
            ("/admin/secret", "/docs/c"),
            ("/docs/c", ""),
        ])
        .await;
        let scope = Scope::new(&CrawlScope {
            include_patterns: vec!["/docs/*".into()],
            exclude_patterns: vec!["*/admin/*".into()],
            ..Default::default()
        });

        let mut counted = Vec::new();
        crawl(
            &site.url("/"),
            &scope,
            LIMITS,
            |page| counted.push(page.url),
            |_| {},
        )
        .await
        .unwrap();

        counted.sort();
        assert_eq!(counted, [site.url("/docs/a"), site.url("/docs/b")]);

        let requested = site.requested_pages();
        assert!(requested.contains(&"/blog/x".to_owned()), "{requested:?}");
        assert!(
            !requested.contains(&"/admin/secret".to_owned()),
            "{requested:?}"
        );
        assert!(!requested.contains(&"/docs/c".to_owned()), "{requested:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn on_fetched_fires_for_a_page_scope_excludes_from_on_counted() {
        let site = TestSite::start([("/", "/docs/a"), ("/docs/a", "")]).await;
        let scope = Scope::new(&CrawlScope {
            include_patterns: vec!["/docs/*".into()],
            ..Default::default()
        });

        let mut counted = Vec::new();
        let mut fetched = Vec::new();
        crawl(
            &site.url("/"),
            &scope,
            LIMITS,
            |page| counted.push(page.url),
            |page| fetched.push(page.final_url),
        )
        .await
        .unwrap();

        assert_eq!(counted, [site.url("/docs/a")]);
        fetched.sort();
        assert_eq!(fetched, [site.url("/"), site.url("/docs/a")]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn on_fetched_never_fires_for_a_url_over_the_byte_limit() {
        let long_path = format!("/{}", "a".repeat(1100));
        let site = TestSite::start([
            ("/", long_path.clone()),
            (long_path.as_str(), String::new()),
        ])
        .await;

        let mut fetched = Vec::new();
        crawl(
            &site.url("/"),
            &everything(),
            LIMITS,
            |_| {},
            |page| fetched.push(page.final_url),
        )
        .await
        .unwrap();

        assert!(fetched.contains(&site.url("/")));
        assert!(!fetched.contains(&site.url(&long_path)), "{fetched:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oversized_html_is_reachable_but_never_counted_or_forwarded() {
        let oversized_html = "x".repeat(crate::links::MAX_PARSEABLE_HTML_BYTES + 1);
        let site = TestSite::start_with_html([("/", oversized_html)]).await;
        let mut counted = 0;
        let mut fetched = 0;

        let result = crawl(
            &site.url("/"),
            &everything(),
            LIMITS,
            |_| counted += 1,
            |_| fetched += 1,
        )
        .await;

        assert_eq!(result, Ok(()));
        assert_eq!(counted, 0);
        assert_eq!(fetched, 0);
        assert_eq!(site.requested_pages(), ["/"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stops_fetching_at_the_page_limit() {
        let many_links: String = (1..=20).map(|i| format!("/p/{i} ")).collect();
        let site = TestSite::start(
            std::iter::once(("/".to_owned(), many_links))
                .chain((1..=20).map(|i| (format!("/p/{i}"), String::new()))),
        )
        .await;
        let limits = Limits {
            max_pages: 5,
            ..LIMITS
        };

        let mut counted = 0;
        crawl(
            &site.url("/"),
            &everything(),
            limits,
            |_| counted += 1,
            |_| {},
        )
        .await
        .unwrap();

        let fetched = site.requested_pages().len();
        assert!((1..=5).contains(&fetched), "fetched {fetched} pages");
        assert_eq!(counted, fetched);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unreachable_site_is_an_error() {
        let result = crawl(
            &unreachable_url().await,
            &everything(),
            LIMITS,
            |_| {},
            |_| {},
        )
        .await;

        assert_eq!(result, Err(Unreachable));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn site_whose_start_page_is_missing_is_an_error() {
        let site = TestSite::start([("/elsewhere", "")]).await;

        let mut counted = 0;
        let result = crawl(
            &site.url("/"),
            &everything(),
            LIMITS,
            |_| counted += 1,
            |_| {},
        )
        .await;

        assert_eq!(result, Err(Unreachable));
        assert_eq!(counted, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn counted_pages_carry_their_html_and_status() {
        let site = TestSite::start([("/", "/docs/a"), ("/docs/a", "")]).await;

        let mut pages = Vec::new();
        crawl(
            &site.url("/"),
            &everything(),
            LIMITS,
            |page| pages.push(page),
            |_| {},
        )
        .await
        .unwrap();

        assert!(
            pages
                .iter()
                .any(|page| page.status == 200 && page.html.contains(r#"<a href="/docs/a">"#)),
            "no counted page carried the root's HTML"
        );
    }
}
