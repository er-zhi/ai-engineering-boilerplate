// Runs one crawl with spider, reporting in-scope pages and every fetched page separately.

use std::time::Duration;

use spider::compact_str::CompactString;
use spider::page::Page;
use spider::website::Website;
use tokio::sync::mpsc;

use crate::scope::Scope;

const USER_AGENT: &str = "ai-engineering-boilerplate-crawler/0.1";
pub(crate) const FETCH_CONCURRENCY: usize = 2;
const FETCHED_PAGE_BUFFER_PAGES: usize = 1;

#[derive(Clone, Copy)]
pub struct Limits {
    pub max_pages: u32,
    pub request_timeout: Duration,
}

#[derive(Debug, PartialEq)]
pub enum CrawlError {
    Unreachable,
    PagesLost(u32),
}

pub struct CountedPage {
    pub url: String,
    pub html: String,
    pub status: u16,
}

pub struct FetchedPage {
    pub final_url: String,
    pub html: String,
}

#[derive(Default)]
struct CrawlProgress {
    any_fetched: bool,
    pages_emitted: u32,
    pages_lost: u32,
}

async fn forward_page(
    page: Page,
    scope: &Scope,
    max_pages: u32,
    progress: &mut CrawlProgress,
    counted: &mpsc::Sender<CountedPage>,
    fetched: &mpsc::Sender<FetchedPage>,
) {
    if !page.status_code.is_success() {
        return;
    }
    progress.any_fetched = true;
    if progress.pages_emitted >= max_pages {
        progress.pages_lost = progress.pages_lost.saturating_add(1);
        return;
    }
    progress.pages_emitted += 1;
    let html = page.get_html();
    if html.len() > crate::links::MAX_PARSEABLE_HTML_BYTES {
        return;
    }
    if scope.counts(page.get_url())
        && counted
            .send(CountedPage {
                url: page.get_url().to_owned(),
                html: html.clone(),
                status: page.status_code.as_u16(),
            })
            .await
            .is_err()
    {
        progress.pages_lost = progress.pages_lost.saturating_add(1);
    }
    let final_url = page.get_url_final().to_owned();
    if final_url.len() <= crate::links::MAX_URL_BYTES
        && fetched.send(FetchedPage { final_url, html }).await.is_err()
    {
        progress.pages_lost = progress.pages_lost.saturating_add(1);
    }
}

pub async fn crawl_into(
    base_url: &str,
    scope: &Scope,
    limits: Limits,
    counted: mpsc::Sender<CountedPage>,
    fetched: mpsc::Sender<FetchedPage>,
) -> Result<(), CrawlError> {
    let mut website = configured_website(base_url, scope, limits);
    let (page_sender, mut pages) = mpsc::channel(FETCHED_PAGE_BUFFER_PAGES);
    website.with_on_should_crawl_callback_closure(Some(move |page: &Page| {
        let page_sender = page_sender.clone();
        let page = page.clone();
        tokio::task::block_in_place(move || page_sender.blocking_send(page).is_ok())
    }));

    let mut progress = CrawlProgress::default();

    let crawling = website.crawl();
    tokio::pin!(crawling);
    loop {
        tokio::select! {
            () = &mut crawling => break,
            received = pages.recv() => match received {
                Some(page) => {
                    forward_page(page, scope, limits.max_pages, &mut progress, &counted, &fetched)
                        .await;
                }
                None => unreachable!(
                    "page_sender lives in the closure `website` owns until `crawling` resolves, \
                     and that arm breaks the loop first"
                ),
            },
        }
    }
    while let Ok(page) = pages.try_recv() {
        forward_page(
            page,
            scope,
            limits.max_pages,
            &mut progress,
            &counted,
            &fetched,
        )
        .await;
    }

    if progress.pages_lost > 0 {
        Err(CrawlError::PagesLost(progress.pages_lost))
    } else if progress.any_fetched {
        Ok(())
    } else {
        Err(CrawlError::Unreachable)
    }
}

fn configured_website(base_url: &str, scope: &Scope, limits: Limits) -> Website {
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
        .with_concurrency_limit(Some(FETCH_CONCURRENCY))
        .with_request_timeout(Some(limits.request_timeout))
        .with_user_agent(Some(USER_AGENT))
        .with_blacklist_url(Some(blacklist));
    website
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

    async fn crawl(
        base_url: &str,
        scope: &Scope,
        limits: Limits,
        mut on_counted: impl FnMut(CountedPage),
        mut on_fetched: impl FnMut(FetchedPage),
    ) -> Result<(), CrawlError> {
        let (counted, mut counted_pages) = mpsc::channel(1);
        let (fetched, mut fetched_pages) = mpsc::channel(1);
        let crawling = crawl_into(base_url, scope, limits, counted, fetched);
        let collecting_counted = async {
            while let Some(page) = counted_pages.recv().await {
                on_counted(page);
            }
        };
        let collecting_fetched = async {
            while let Some(page) = fetched_pages.recv().await {
                on_fetched(page);
            }
        };
        let (result, (), ()) = tokio::join!(crawling, collecting_counted, collecting_fetched);
        result
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
    async fn a_closed_pipeline_receiver_reports_the_page_as_lost() {
        let site = TestSite::start([("/", "page")]).await;
        let (counted, counted_pages) = mpsc::channel(1);
        let (fetched, _fetched_pages) = mpsc::channel(1);
        drop(counted_pages);

        let result = crawl_into(&site.url("/"), &everything(), LIMITS, counted, fetched).await;

        assert!(matches!(result, Err(CrawlError::PagesLost(pages)) if pages >= 1));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn slow_pipeline_applies_backpressure_without_losing_pages() {
        let links: String = (1..=20).map(|i| format!("/p/{i} ")).collect();
        let site = TestSite::start(
            std::iter::once(("/".to_owned(), links))
                .chain((1..=20).map(|i| (format!("/p/{i}"), String::new()))),
        )
        .await;
        let (counted, mut counted_pages) = mpsc::channel(1);
        let (fetched, mut fetched_pages) = mpsc::channel(1);
        let base_url = site.url("/");
        let scope = everything();
        let crawling = crawl_into(&base_url, &scope, LIMITS, counted, fetched);
        let counted = async {
            let mut count = 0;
            while counted_pages.recv().await.is_some() {
                tokio::time::sleep(Duration::from_millis(10)).await;
                count += 1;
            }
            count
        };
        let fetched = async {
            let mut count = 0;
            while fetched_pages.recv().await.is_some() {
                tokio::time::sleep(Duration::from_millis(10)).await;
                count += 1;
            }
            count
        };

        let (result, counted, fetched) = tokio::join!(crawling, counted, fetched);

        assert_eq!(result, Ok(()));
        assert_eq!(counted, 21);
        assert_eq!(fetched, 21);
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

        assert_eq!(result, Err(CrawlError::Unreachable));
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

        assert_eq!(result, Err(CrawlError::Unreachable));
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
