// One JSON Schema annotation that two services have to agree on: Tool writes it into a tool's
// `input_schema_json`, Engine reads it back out of the catalog. It travels inside a string field of
// `tools.v1.Tool`, so protobuf cannot carry the agreement and this constant does.

/// Marks an argument whose value is free text — a query written by whoever is asking, where the
/// user's own words *are* the value. Engine may then hand such an argument the message verbatim and
/// skip the generative call that would otherwise compose it.
///
/// **Absent means no.** An argument that does not say this takes a value rather than a query — a
/// place, a pair, a code — and handing it a whole sentence produces a request that was never going
/// to work: measured at ~850 ms spent on a third-party rejection, against ~321 ms for the
/// generative call that composes the argument correctly. The default is therefore closed, and the
/// cost of forgetting the annotation on a genuinely free-text argument is one composed call rather
/// than a wasted round trip.
pub const ACCEPTS_FREE_TEXT: &str = "x-accepts-free-text";

/// Marks an argument whose value is a *word or two of the message itself* rather than the whole of
/// it — a place, a pair, a code. Engine may then offer the message's own words as options to a
/// typed decision and copy the one it picks, instead of spending a generative call composing the
/// argument. The value that comes back is a span of what the person wrote: the decision can only
/// choose among the options it was handed, so it cannot invent one.
///
/// Distinct from [`ACCEPTS_FREE_TEXT`], which says the argument takes the message entire. An
/// argument may say neither, and then nothing is guessed and the argument is composed as before.
pub const VALUE_IN_MESSAGE: &str = "x-value-in-message";

/// Whether `property` — one entry of a JSON Schema's `properties` — is annotated as free text.
#[must_use]
pub fn accepts_free_text(property: &serde_json::Value) -> bool {
    flagged(property, ACCEPTS_FREE_TEXT)
}

/// Whether `property` says its value appears among the words of the message.
#[must_use]
pub fn value_in_message(property: &serde_json::Value) -> bool {
    flagged(property, VALUE_IN_MESSAGE)
}

fn flagged(property: &serde_json::Value, key: &str) -> bool {
    property
        .get(key)
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
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

    /// A non-boolean is not a quiet yes: an operator writing `"true"` gets the closed default, which
    /// costs one composed call, rather than the wasted round trip a lenient read would buy them.
    #[test]
    fn a_non_boolean_annotation_does_not() {
        assert!(!accepts_free_text(
            &json!({"type": "string", ACCEPTS_FREE_TEXT: "true"})
        ));
    }

    /// The two say different things and must not be read for each other: one takes the message
    /// entire, the other takes a word or two out of it.
    #[test]
    fn the_two_annotations_are_read_separately() {
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
