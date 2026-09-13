// Extracts outbound <a href> links from a page's raw HTML for graph-edge discovery.

use scraper::{Html, Selector};
use url::Url;

const MAX_LINKS_PER_PAGE: usize = 500;
pub const MAX_URL_BYTES: usize = 1024;
pub const MAX_ANCHOR_TEXT_CHARS: usize = 512;
const MAX_ANCHOR_TEXT_RAW_CHARS: usize = MAX_ANCHOR_TEXT_CHARS * 4;
pub(crate) const MAX_PARSEABLE_HTML_BYTES: usize = 5 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub url: String,
    pub anchor_text: String,
}

pub fn try_extract_links(base_url: &str, html: &str) -> Option<Vec<Link>> {
    if html.len() > MAX_PARSEABLE_HTML_BYTES {
        return None;
    }
    let base_url = Url::parse(base_url).ok()?;
    let document = Html::parse_document(html);
    let base = document_base_url(&document, &base_url);

    let anchor_selector = Selector::parse("a[href]").expect("static selector is valid");
    let mut seen = std::collections::HashSet::new();
    let mut links = Vec::new();

    for element in document.select(&anchor_selector) {
        let Some(href) = element.value().attr("href") else {
            continue;
        };
        let Some(resolved) = resolve_same_scheme_url(&base, href) else {
            continue;
        };
        if resolved == base_url.as_str() || !seen.insert(resolved.clone()) {
            continue;
        }
        let raw_text: String = element
            .text()
            .flat_map(str::chars)
            .take(MAX_ANCHOR_TEXT_RAW_CHARS)
            .collect();
        links.push(Link {
            url: resolved,
            anchor_text: collapse_and_cap_whitespace(&raw_text),
        });
        if links.len() >= MAX_LINKS_PER_PAGE {
            break;
        }
    }
    Some(links)
}

fn document_base_url(document: &Html, page_url: &Url) -> Url {
    let base_selector = Selector::parse("base[href]").expect("static selector is valid");
    document
        .select(&base_selector)
        .find_map(|element| element.value().attr("href"))
        .and_then(|href| page_url.join(href).ok())
        .unwrap_or_else(|| page_url.clone())
}

fn resolve_same_scheme_url(base: &Url, href: &str) -> Option<String> {
    let mut joined = base.join(href).ok()?;
    if !matches!(joined.scheme(), "http" | "https") {
        return None;
    }
    joined.set_fragment(None);
    let normalized = joined.to_string();
    (normalized.len() <= MAX_URL_BYTES).then_some(normalized)
}

fn collapse_and_cap_whitespace(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(MAX_ANCHOR_TEXT_CHARS)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_relative_links_against_the_page_url() {
        let links = try_extract_links(
            "https://example.com/docs/a",
            r#"<a href="/docs/b">Docs B</a>"#,
        )
        .unwrap();

        assert_eq!(
            links,
            [Link {
                url: "https://example.com/docs/b".to_owned(),
                anchor_text: "Docs B".to_owned(),
            }]
        );
    }

    #[test]
    fn honors_a_base_href_over_the_page_url() {
        let links = try_extract_links(
            "https://example.com/docs/a",
            r#"<base href="https://other.example.com/blog/"><a href="post">Post</a>"#,
        )
        .unwrap();

        assert_eq!(links[0].url, "https://other.example.com/blog/post");
    }

    #[test]
    fn drops_a_self_link_to_the_page_itself() {
        let links = try_extract_links(
            "https://example.com/docs/a",
            r#"<a href="/docs/a">Here</a><a href="/docs/a#section">Also here</a>"#,
        )
        .unwrap();

        assert!(links.is_empty(), "{links:?}");
    }

    #[test]
    fn strips_the_fragment_from_an_otherwise_distinct_link() {
        let links =
            try_extract_links("https://example.com/", r#"<a href="/docs/a#intro">A</a>"#).unwrap();

        assert_eq!(links[0].url, "https://example.com/docs/a");
    }

    #[test]
    fn drops_non_http_schemes() {
        let links = try_extract_links(
            "https://example.com/",
            r#"<a href="mailto:a@example.com">Mail</a><a href="javascript:void(0)">JS</a><a href="/ok">OK</a>"#,
        )
        .unwrap();

        assert_eq!(links.len(), 1);
        assert_eq!(links[0].url, "https://example.com/ok");
    }

    #[test]
    fn deduplicates_repeated_links_keeping_the_first_anchor_text() {
        let links = try_extract_links(
            "https://example.com/",
            r#"<a href="/a">First</a><a href="/a">Second</a>"#,
        )
        .unwrap();

        assert_eq!(links.len(), 1);
        assert_eq!(links[0].anchor_text, "First");
    }

    #[test]
    fn collapses_whitespace_heavy_anchor_text() {
        let links = try_extract_links(
            "https://example.com/",
            "<a href=\"/a\">  Wireless \n  Headphones  </a>",
        )
        .unwrap();

        assert_eq!(links[0].anchor_text, "Wireless Headphones");
    }

    #[test]
    fn caps_anchor_text_at_the_column_limit() {
        let long_word = "a".repeat(MAX_ANCHOR_TEXT_CHARS + 200);
        let html = format!("<a href=\"/a\">{long_word}</a>");

        let links = try_extract_links("https://example.com/", &html).unwrap();

        assert_eq!(links[0].anchor_text.chars().count(), MAX_ANCHOR_TEXT_CHARS);
    }

    #[test]
    fn an_unparseable_base_url_skips_extraction_rather_than_panicking() {
        assert_eq!(try_extract_links("not a url", "<a href=\"/a\">A</a>"), None);
    }

    #[test]
    fn a_page_with_no_links_extracts_an_empty_set() {
        assert_eq!(
            try_extract_links("https://example.com/", "<p>No links here.</p>"),
            Some(Vec::new())
        );
    }

    #[test]
    fn stops_at_the_per_page_link_cap() {
        let html: String = (0..MAX_LINKS_PER_PAGE + 50)
            .map(|n| format!(r#"<a href="/p/{n}">{n}</a>"#))
            .collect();

        let links = try_extract_links("https://example.com/", &html).unwrap();

        assert_eq!(links.len(), MAX_LINKS_PER_PAGE);
    }

    #[test]
    fn html_at_exactly_the_parseable_size_still_extracts() {
        const FIXED_MARKUP_BYTES: usize = "<a href=\"/a\">A</a><!---->".len();
        let filler = "x".repeat(MAX_PARSEABLE_HTML_BYTES - FIXED_MARKUP_BYTES);
        let html = format!("<a href=\"/a\">A</a><!--{filler}-->");
        assert_eq!(html.len(), MAX_PARSEABLE_HTML_BYTES);

        let links = try_extract_links("https://example.com/", &html).unwrap();

        assert_eq!(links.len(), 1);
    }

    #[test]
    fn html_over_the_parseable_size_skips_extraction_rather_than_parsing() {
        let filler = "x".repeat(MAX_PARSEABLE_HTML_BYTES + 1);
        let html = format!("<a href=\"/a\">A</a><!--{filler}-->");

        assert_eq!(try_extract_links("https://example.com/", &html), None);
    }
}
