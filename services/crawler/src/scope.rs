// Crawl scope: which URLs are never fetched and which fetched pages count toward a job.

use common::proto::crawler::v1::CrawlScope;
use regex::Regex;

pub struct Scope {
    include: Vec<Regex>,
    exclude: Vec<Regex>,
    include_urls: Vec<String>,
    exclude_urls: Vec<String>,
}

impl Scope {
    pub fn new(rules: &CrawlScope) -> Self {
        Self {
            include: rules
                .include_patterns
                .iter()
                .map(|p| regex_from_glob(p))
                .collect(),
            exclude: rules
                .exclude_patterns
                .iter()
                .map(|p| regex_from_glob(p))
                .collect(),
            include_urls: rules.include_urls.clone(),
            exclude_urls: rules.exclude_urls.clone(),
        }
    }

    pub fn blocks(&self, url: &str) -> bool {
        self.exclude_urls.iter().any(|excluded| excluded == url)
            || self.exclude.iter().any(|pattern| pattern.is_match(url))
    }

    pub fn counts(&self, url: &str) -> bool {
        if self.blocks(url) {
            return false;
        }
        if self.include.is_empty() && self.include_urls.is_empty() {
            return true;
        }
        self.include_urls.iter().any(|included| included == url)
            || self.include.iter().any(|pattern| pattern.is_match(url))
    }

    pub fn blacklist(&self) -> Vec<String> {
        let patterns = self
            .exclude
            .iter()
            .map(|pattern| pattern.as_str().to_owned());
        let exact = self
            .exclude_urls
            .iter()
            .map(|url| format!("^{}$", regex::escape(url)));
        patterns.chain(exact).collect()
    }
}

fn regex_from_glob(pattern: &str) -> Regex {
    let body = pattern
        .split('*')
        .map(regex::escape)
        .collect::<Vec<_>>()
        .join(".*");
    let origin = if pattern.starts_with('/') {
        "[^:/]+://[^/]+"
    } else {
        ""
    };
    Regex::new(&format!("^{origin}{body}$")).expect("an escaped glob is always a valid regex")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules() -> CrawlScope {
        CrawlScope::default()
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| item.to_string()).collect()
    }

    #[test]
    fn every_page_counts_when_no_includes_are_given() {
        let scope = Scope::new(&rules());

        assert!(scope.counts("https://example.com/"));
        assert!(scope.counts("https://example.com/any/deep/page?x=1"));
    }

    #[test]
    fn path_pattern_is_anchored_at_the_start_of_the_path() {
        let scope = Scope::new(&CrawlScope {
            include_patterns: strings(&["/docs/*"]),
            ..rules()
        });

        for (url, want) in [
            ("https://example.com/docs/intro", true),
            ("https://example.com/docs/a/b", true),
            ("http://127.0.0.1:8080/docs/a", true),
            ("https://example.com/", false),
            ("https://example.com/blog/docs/x", false),
            ("https://example.com/docsx", false),
        ] {
            assert_eq!(scope.counts(url), want, "{url}");
        }
    }

    #[test]
    fn excluded_urls_are_blocked_and_never_counted() {
        let scope = Scope::new(&CrawlScope {
            exclude_patterns: strings(&["*/admin/*"]),
            ..rules()
        });

        for (url, want_blocked) in [
            ("https://example.com/admin/users", true),
            ("https://example.com/en/admin/x", true),
            ("https://example.com/administrator", false),
        ] {
            assert_eq!(scope.blocks(url), want_blocked, "{url}");
            assert_eq!(scope.counts(url), !want_blocked, "{url}");
        }
    }

    #[test]
    fn exclude_wins_over_include() {
        let scope = Scope::new(&CrawlScope {
            include_patterns: strings(&["/docs/*"]),
            exclude_patterns: strings(&["/docs/internal/*"]),
            ..rules()
        });

        assert!(scope.counts("https://example.com/docs/public"));
        assert!(!scope.counts("https://example.com/docs/internal/plan"));
    }

    #[test]
    fn include_urls_alone_restrict_counting_to_exactly_those_urls() {
        let scope = Scope::new(&CrawlScope {
            include_urls: strings(&["https://example.com/pricing"]),
            ..rules()
        });

        assert!(scope.counts("https://example.com/pricing"));
        assert!(!scope.counts("https://example.com/"));
        assert!(!scope.counts("https://example.com/pricing/enterprise"));
    }

    #[test]
    fn include_urls_count_alongside_include_patterns() {
        let scope = Scope::new(&CrawlScope {
            include_patterns: strings(&["/docs/*"]),
            include_urls: strings(&["https://example.com/pricing"]),
            ..rules()
        });

        assert!(scope.counts("https://example.com/docs/a"));
        assert!(scope.counts("https://example.com/pricing"));
        assert!(!scope.counts("https://example.com/blog/x"));
    }

    #[test]
    fn exclude_urls_block_only_the_exact_url() {
        let scope = Scope::new(&CrawlScope {
            exclude_urls: strings(&["https://example.com/private/page"]),
            ..rules()
        });

        assert!(scope.blocks("https://example.com/private/page"));
        assert!(!scope.counts("https://example.com/private/page"));
        assert!(!scope.blocks("https://example.com/private/page2"));
    }

    #[test]
    fn regex_characters_in_patterns_match_literally() {
        let scope = Scope::new(&CrawlScope {
            include_patterns: strings(&["/v1.0/*"]),
            exclude_patterns: strings(&["*?preview=*"]),
            ..rules()
        });

        assert!(scope.counts("https://example.com/v1.0/a"));
        assert!(!scope.counts("https://example.com/v1x0/a"));
        assert!(scope.blocks("https://example.com/v1.0/a?preview=1"));
        assert!(!scope.blocks("https://example.com/v1.0/a"));
    }

    #[test]
    fn blacklist_blocks_the_same_urls_when_compiled_the_way_spider_does() {
        let scope = Scope::new(&CrawlScope {
            exclude_patterns: strings(&["*/admin/*"]),
            exclude_urls: strings(&["https://example.com/private/page"]),
            ..rules()
        });

        let spider_set = regex::RegexSet::new(scope.blacklist()).unwrap();

        assert!(spider_set.is_match("https://example.com/admin/x"));
        assert!(spider_set.is_match("https://example.com/private/page"));
        assert!(!spider_set.is_match("https://example.com/private/page2"));
        assert!(!spider_set.is_match("https://example.com/docs/a"));
    }
}
