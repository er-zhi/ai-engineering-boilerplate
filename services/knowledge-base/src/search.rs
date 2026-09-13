// Turns a raw query into what the retrievers take, and page-type names into the entity enum and back.

use crate::entity::document::PageType;

pub fn normalize(query: &str) -> String {
    query
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

pub fn any_word_lexical_query(query: &str) -> Option<String> {
    let words: Vec<&str> = query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect();
    (!words.is_empty()).then(|| words.join(" | "))
}

pub fn page_type_named(name: &str) -> Option<PageType> {
    match name {
        "product" => Some(PageType::Product),
        "knowledge" => Some(PageType::Knowledge),
        "instruction" => Some(PageType::Instruction),
        "documentation" => Some(PageType::Documentation),
        "blog" => Some(PageType::Blog),
        "other" => Some(PageType::Other),
        _ => None,
    }
}

pub fn page_type_name(page_type: PageType) -> &'static str {
    match page_type {
        PageType::Other => "other",
        PageType::Product => "product",
        PageType::Knowledge => "knowledge",
        PageType::Instruction => "instruction",
        PageType::Documentation => "documentation",
        PageType::Blog => "blog",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_lowercases_and_collapses_whitespace() {
        assert_eq!(
            normalize("  Wireless \t  Headphones\n"),
            "wireless headphones"
        );
    }

    #[test]
    fn normalize_lowercases_beyond_ascii() {
        assert_eq!(normalize("ГИБРИДНЫЙ Поиск"), "гибридный поиск");
    }

    #[test]
    fn any_word_lexical_query_ors_the_words_and_drops_punctuation() {
        assert_eq!(
            any_word_lexical_query("how do I write CLAUDE.md?").as_deref(),
            Some("how | do | I | write | CLAUDE | md")
        );
    }

    #[test]
    fn a_query_with_no_words_has_no_lexical_query() {
        assert_eq!(any_word_lexical_query(" ?! -- "), None);
    }

    #[test]
    fn page_type_names_round_trip() {
        for page_type in [
            PageType::Other,
            PageType::Product,
            PageType::Knowledge,
            PageType::Instruction,
            PageType::Documentation,
            PageType::Blog,
        ] {
            assert_eq!(page_type_named(page_type_name(page_type)), Some(page_type));
        }
    }
}
