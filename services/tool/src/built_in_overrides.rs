// What the operator says about the tools compiled into this binary: what each one is called, what it covers, and whether it is in circulation.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::service::{MAX_DESCRIPTION_CHARS, MAX_NAME_CHARS};

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Override {
    pub name: Option<String>,
    pub description: Option<String>,
    pub enabled: Option<bool>,
}

#[must_use]
pub fn load(path: &str) -> BTreeMap<String, Override> {
    let none_applied_rather_than_half = BTreeMap::new();
    match read_file(path) {
        Ok(overrides) => overrides,
        Err(error) => {
            tracing::error!(path, %error, "built-in tool overrides not applied");
            none_applied_rather_than_half
        }
    }
}

fn read_file(path: &str) -> Result<BTreeMap<String, Override>, String> {
    let raw = std::fs::read_to_string(path).map_err(|error| error.to_string())?;
    let overrides = read(&raw)?;
    tracing::info!(
        path,
        count = overrides.len(),
        "loaded built-in tool overrides"
    );
    Ok(overrides)
}

fn read(raw: &str) -> Result<BTreeMap<String, Override>, String> {
    let overrides: BTreeMap<String, Override> =
        serde_json::from_str(raw).map_err(|error| error.to_string())?;
    for (slug, applied) in &overrides {
        within(slug, "name", applied.name.as_deref(), MAX_NAME_CHARS)?;
        within(
            slug,
            "description",
            applied.description.as_deref(),
            MAX_DESCRIPTION_CHARS,
        )?;
    }
    Ok(overrides)
}

fn within(slug: &str, field: &str, value: Option<&str>, max_chars: usize) -> Result<(), String> {
    let Some(value) = value else {
        return Ok(());
    };
    let chars = value.chars().count();
    if chars > max_chars {
        return Err(format!(
            "{slug}'s {field} is {chars} characters, over the {max_chars} this service stores"
        ));
    }
    Ok(())
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

    #[test]
    fn a_misspelled_field_is_refused_rather_than_silently_dropped() {
        assert!(read(r#"{"kb_search": {"describtion": "typo"}}"#).is_err());
    }

    #[test]
    fn a_description_past_what_the_service_stores_refuses_the_file() {
        let too_long = "x".repeat(MAX_DESCRIPTION_CHARS + 1);
        let refusal = read(&format!(
            r#"{{"kb_search": {{"description": "{too_long}"}}}}"#
        ))
        .expect_err("a description this service cannot store is refused");

        assert!(
            refusal.contains("description") && refusal.contains(&MAX_DESCRIPTION_CHARS.to_string())
        );
    }

    #[test]
    fn a_name_past_its_column_refuses_the_file() {
        let too_long = "x".repeat(MAX_NAME_CHARS + 1);
        assert!(read(&format!(r#"{{"kb_search": {{"name": "{too_long}"}}}}"#)).is_err());
    }

    #[test]
    fn an_unreadable_file_leaves_the_binarys_own_declarations_standing() {
        assert!(load("/nonexistent/built-in-tools.json").is_empty());
    }
}
