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

/// Whether `property` — one entry of a JSON Schema's `properties` — is annotated as free text.
#[must_use]
pub fn accepts_free_text(property: &serde_json::Value) -> bool {
    property
        .get(ACCEPTS_FREE_TEXT)
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
}
