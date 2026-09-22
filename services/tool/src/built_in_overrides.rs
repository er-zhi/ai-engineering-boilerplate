// What the operator says about the tools compiled into this binary. The declarative rows next door
// are wholly the operator's; these are not — their code lives here — but which of them are in
// circulation, and how their coverage is described, is an operating decision rather than a
// compile-time one.
//
// It matters because the catalog is what decides whether a question can be answered at all. A
// description that says what a source *covers* lets the typed decision route a question to it, or
// to nothing; one that merely says what the tool *is* leaves the decision no basis, and every
// question it should have answered is declined instead. Measured on a search tool whose stored
// material the binary could not describe: 16 of 16 questions routed correctly once the operator
// wrote what it covers, against nothing reaching it before.
//
// What a store holds changes without the binary changing, and which sources an operator is willing
// to answer from is theirs to set. So both are a file edit, the same way a declarative row is.

use std::collections::BTreeMap;

use serde::Deserialize;

/// What an operator may say about one built-in tool. Every field is optional: an entry states only
/// what it changes, and anything left out keeps what the binary declares.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Override {
    pub name: Option<String>,
    /// What this source covers, written for the decision that routes a question to it or to
    /// nothing. Its subject matter belongs here, in the operator's file, and not in the binary —
    /// see the repository's "Capabilities, Not Topics" rule.
    pub description: Option<String>,
    /// `false` takes the tool out of circulation: it is never seeded, and an already-seeded row is
    /// moved out of the catalog. This is how a source stops being answerable from without a code
    /// change.
    pub enabled: Option<bool>,
}

/// Every override, by slug. A missing file, an unreadable one, or one that will not parse yields
/// none: the binary's own declarations then stand, which is the safe direction — a tool keeps
/// working, and nothing silently loses the description that makes it reachable.
#[must_use]
pub fn load(path: &str) -> BTreeMap<String, Override> {
    match read_file(path) {
        Ok(overrides) => overrides,
        // Refusing the file whole beats applying half of it: a partly-applied file leaves some
        // sources described and others not, and the difference is invisible until a question is
        // routed to nothing.
        Err(error) => {
            tracing::error!(path, %error, "built-in tool overrides not applied");
            BTreeMap::new()
        }
    }
}

fn read_file(path: &str) -> Result<BTreeMap<String, Override>, String> {
    let raw = std::fs::read_to_string(path).map_err(|error| error.to_string())?;
    let overrides = read(&raw).map_err(|error| error.to_string())?;
    tracing::info!(
        path,
        count = overrides.len(),
        "loaded built-in tool overrides"
    );
    Ok(overrides)
}

fn read(raw: &str) -> Result<BTreeMap<String, Override>, serde_json::Error> {
    serde_json::from_str(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(raw: &str) -> BTreeMap<String, Override> {
        read(raw).expect("a usable file")
    }

    #[test]
    fn an_entry_states_only_what_it_changes() {
        let overrides = parsed(r#"{"kb_search": {"description": "covers the manuals"}}"#);
        let entry = &overrides["kb_search"];

        assert_eq!(entry.description.as_deref(), Some("covers the manuals"));
        assert!(
            entry.name.is_none() && entry.enabled.is_none(),
            "what an entry leaves out keeps what the binary declares"
        );
    }

    #[test]
    fn a_tool_can_be_taken_out_of_circulation() {
        let overrides = parsed(r#"{"web_search": {"enabled": false}}"#);
        assert_eq!(overrides["web_search"].enabled, Some(false));
    }

    /// A misspelled field is a description that will not arrive, and a description that does not
    /// arrive is a question that cannot be answered. The operator hears about it at startup rather
    /// than discovering it as a decline weeks later.
    #[test]
    fn a_field_this_does_not_know_is_refused() {
        assert!(read(r#"{"kb_search": {"describtion": "typo"}}"#).is_err());
    }

    #[test]
    fn an_unreadable_file_leaves_the_binarys_own_declarations_standing() {
        assert!(load("/nonexistent/built-in-tools.json").is_empty());
    }
}
