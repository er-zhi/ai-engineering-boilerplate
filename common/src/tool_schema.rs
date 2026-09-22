// The JSON Schema annotations Tool writes into a tool's `input_schema_json` and Engine reads back out of the catalog: one says an argument takes the whole message, the other that its value is a word or two inside it.

pub const ACCEPTS_FREE_TEXT: &str = "x-accepts-free-text";

pub const VALUE_IN_MESSAGE: &str = "x-value-in-message";

#[must_use]
pub fn accepts_free_text(property: &serde_json::Value) -> bool {
    flagged(property, ACCEPTS_FREE_TEXT)
}

#[must_use]
pub fn value_in_message(property: &serde_json::Value) -> bool {
    flagged(property, VALUE_IN_MESSAGE)
}

fn flagged(property: &serde_json::Value, key: &str) -> bool {
    const ABSENT_OR_UNREADABLE_MEANS_NO: bool = false;
    property
        .get(key)
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(ABSENT_OR_UNREADABLE_MEANS_NO)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_annotated_property_accepts_free_text() {
        assert!(accepts_free_text(
            &json!({"type": "string", ACCEPTS_FREE_TEXT: true})
        ));
    }

    #[test]
    fn an_unannotated_property_does_not() {
        assert!(!accepts_free_text(&json!({"type": "string"})));
    }

    #[test]
    fn the_annotation_set_to_false_does_not() {
        assert!(!accepts_free_text(
            &json!({"type": "string", ACCEPTS_FREE_TEXT: false})
        ));
    }

    #[test]
    fn a_non_boolean_annotation_is_not_a_quiet_yes() {
        assert!(!accepts_free_text(
            &json!({"type": "string", ACCEPTS_FREE_TEXT: "true"})
        ));
    }

    #[test]
    fn the_annotation_for_the_whole_message_and_the_one_for_a_word_in_it_are_read_separately() {
        let whole = json!({"type": "string", ACCEPTS_FREE_TEXT: true});
        let part = json!({"type": "string", VALUE_IN_MESSAGE: true});

        assert!(accepts_free_text(&whole) && !value_in_message(&whole));
        assert!(value_in_message(&part) && !accepts_free_text(&part));
    }

    #[test]
    fn an_argument_that_says_neither_gets_neither() {
        let plain = json!({"type": "string"});
        assert!(!accepts_free_text(&plain) && !value_in_message(&plain));
    }
}
