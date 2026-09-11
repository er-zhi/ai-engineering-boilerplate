// Turns fetched HTML into what the crawler keeps: a title, the main text, and a hash of that text.
// Raw HTML never leaves this module.
// Only tests call this module until the page store uses it; remove this allow then.
#![cfg_attr(not(test), allow(dead_code))]

use dom_smoothie::Readability;
use sha2::{Digest, Sha256};

/// `crawler.pages.title` is varchar(512).
const MAX_TITLE_CHARS: usize = 512;

#[derive(Debug, PartialEq)]
pub struct Extracted {
    pub title: String,
    pub main_text: String,
    /// Lowercase hex SHA-256 of `main_text`.
    pub content_hash: String,
}

/// Pages without a recognizable article keep their `<title>` and get empty text.
pub fn extract(url: &str, html: &str) -> Extracted {
    let (title, text) = match Readability::new(html, Some(url), None) {
        Ok(mut readability) => {
            let fallback_title = readability.get_article_title().to_string();
            match readability.parse() {
                Ok(article) => (article.title, article.text_content.to_string()),
                Err(_) => (fallback_title, String::new()),
            }
        }
        Err(_) => (String::new(), String::new()),
    };
    let main_text = tidy(&text);

    Extracted {
        title: title.trim().chars().take(MAX_TITLE_CHARS).collect(),
        content_hash: sha256_hex(&main_text),
        main_text,
    }
}

/// Trims each line and drops blank ones, so whitespace churn alone doesn't change the hash.
fn tidy(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn sha256_hex(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ARTICLE_PAGE: &str = r#"<!doctype html><html><head><title>Rust Ownership Guide</title></head><body>
<nav><a href="/">Home</a> <a href="/pricing">Pricing</a> Sign in</nav>
<article><h1>Rust Ownership Guide</h1>
<p>Ownership is a set of rules that govern how a Rust program manages memory. Each value has exactly one owner, and the value is dropped when its owner goes out of scope.</p>
<p>Borrowing lets code use a value without taking ownership of it. References must always be valid, which the borrow checker enforces at compile time.</p>
<p>Moves transfer ownership between variables, so the original binding can no longer be used after the move happens in the program.</p>
</article>
<footer>Copyright 2026 Example Corp. Privacy policy. Cookie settings.</footer>
</body></html>"#;

    const EMPTY_PAGE: &str =
        "<!doctype html><html><head><title>Empty</title></head><body></body></html>";

    #[test]
    fn keeps_the_article_text_and_drops_navigation_and_footer() {
        let page = extract("https://example.com/guide", ARTICLE_PAGE);

        assert_eq!(page.title, "Rust Ownership Guide");
        assert!(
            page.main_text.contains("Each value has exactly one owner"),
            "{}",
            page.main_text
        );
        assert!(
            page.main_text.contains("borrow checker enforces"),
            "{}",
            page.main_text
        );
        for boilerplate in ["Pricing", "Sign in", "Cookie settings"] {
            assert!(
                !page.main_text.contains(boilerplate),
                "kept {boilerplate:?}: {}",
                page.main_text
            );
        }
    }

    #[test]
    fn page_without_an_article_keeps_its_title_and_has_no_text() {
        let page = extract("https://example.com/", EMPTY_PAGE);

        assert_eq!(page.title, "Empty");
        assert_eq!(page.main_text, "");
    }

    #[test]
    fn content_hash_is_the_sha256_of_the_main_text() {
        // Published SHA-256 test vectors, so the expected values don't come from our own code.
        assert_eq!(
            extract("https://example.com/", EMPTY_PAGE).content_hash,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn titles_longer_than_the_column_are_cut_to_512_characters() {
        let long = "é".repeat(600);
        let html = format!("<html><head><title>{long}</title></head><body></body></html>");

        let page = extract("https://example.com/", &html);

        assert_eq!(page.title.chars().count(), 512);
    }
}
