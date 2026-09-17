// Turns a raw query into what the retrievers take, and the published page type into the entity enum and back.

use buffa::Enumeration;
use common::proto::knowledge_base::v1::PageType as PageTypeProto;

use crate::entity::document::PageType;

const PAGE_TYPE_NAME_PREFIX: &str = "PAGE_TYPE_";

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

pub fn page_type_of(declared: PageTypeProto) -> Option<PageType> {
    match declared {
        PageTypeProto::Unspecified => None,
        PageTypeProto::Other => Some(PageType::Other),
        PageTypeProto::Product => Some(PageType::Product),
        PageTypeProto::Knowledge => Some(PageType::Knowledge),
        PageTypeProto::Instruction => Some(PageType::Instruction),
        PageTypeProto::Documentation => Some(PageType::Documentation),
        PageTypeProto::Blog => Some(PageType::Blog),
    }
}

pub fn page_type_proto(page_type: PageType) -> PageTypeProto {
    match page_type {
        PageType::Other => PageTypeProto::Other,
        PageType::Product => PageTypeProto::Product,
        PageType::Knowledge => PageTypeProto::Knowledge,
        PageType::Instruction => PageTypeProto::Instruction,
        PageType::Documentation => PageTypeProto::Documentation,
        PageType::Blog => PageTypeProto::Blog,
    }
}

pub fn page_type_word(declared: PageTypeProto) -> String {
    declared
        .proto_name()
        .trim_start_matches(PAGE_TYPE_NAME_PREFIX)
        .to_lowercase()
}

pub fn page_type_of_word(word: &str) -> Option<PageTypeProto> {
    PageTypeProto::from_proto_name(&format!(
        "{PAGE_TYPE_NAME_PREFIX}{}",
        word.trim().to_uppercase()
    ))
}

pub fn classifiable_page_types() -> Vec<PageTypeProto> {
    PageTypeProto::values()
        .iter()
        .copied()
        .filter(|declared| *declared != PageTypeProto::Unspecified)
        .collect()
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
    fn the_word_a_classifier_answers_with_names_the_page_type_it_came_from() {
        for declared in classifiable_page_types() {
            assert_eq!(page_type_of_word(&page_type_word(declared)), Some(declared));
        }
        assert_eq!(page_type_of_word("faq"), None);
    }
}
