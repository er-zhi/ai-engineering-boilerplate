// A tool whose behaviour is a registry row rather than Rust: several endpoints that answer the same
// question, raced through `crate::race`. Nothing here knows what any of them is about — the subject
// lives in the row's description, the same place a user-created tool's does.

use serde::Deserialize;
use serde_json::Value;

const PLACEHOLDER_OPEN: char = '{';
const PLACEHOLDER_CLOSE: char = '}';

#[derive(Debug, Deserialize)]
pub struct SourceSet {
    /// How many sources run at once. The rest wait as reserve.
    pub fan_out: usize,
    /// How many answers are enough. Reaching it cancels whatever is still in flight.
    pub take: usize,
    pub sources: Vec<Source>,
}

#[derive(Debug, Deserialize)]
pub struct Source {
    pub name: String,
    /// `{field}` placeholders are filled from the tool's input, and only from fields its
    /// `input_schema` declares.
    pub url: String,
    /// Dotted path to the value in the source's JSON reply.
    pub pick: String,
}

/// Reads and checks a row's `sources`. Every failure here is a seeding mistake, so seeding calls it
/// too (Task 5) and refuses to write a row it rejects — otherwise the first user to reach the tool
/// is the one who finds out.
pub fn parse_sources(raw: &Value, input_schema: &Value) -> Result<SourceSet, String> {
    let set: SourceSet =
        serde_json::from_value(raw.clone()).map_err(|error| format!("sources: {error}"))?;
    if set.sources.is_empty() {
        return Err("sources must list at least one source".to_owned());
    }
    if set.take == 0 || set.take > set.sources.len() {
        return Err(format!(
            "take must be between 1 and the {} sources listed, got {}",
            set.sources.len(),
            set.take
        ));
    }
    if set.fan_out == 0 {
        return Err("fan_out must be at least 1".to_owned());
    }
    let declared = declared_fields(input_schema);
    for source in &set.sources {
        for field in placeholders(&source.url) {
            if !declared.contains(&field) {
                return Err(format!(
                    "source {:?} uses {{{field}}}, which input_schema does not declare",
                    source.name
                ));
            }
        }
    }
    Ok(set)
}

fn declared_fields(input_schema: &Value) -> Vec<String> {
    input_schema
        .get("properties")
        .and_then(Value::as_object)
        .map(|properties| properties.keys().cloned().collect())
        .unwrap_or_default()
}

fn placeholders(template: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = template;
    while let Some(start) = rest.find(PLACEHOLDER_OPEN) {
        let after = &rest[start + PLACEHOLDER_OPEN.len_utf8()..];
        match after.find(PLACEHOLDER_CLOSE) {
            Some(end) => {
                found.push(after[..end].to_owned());
                rest = &after[end + PLACEHOLDER_CLOSE.len_utf8()..];
            }
            None => break,
        }
    }
    found
}

/// Substitutes every `{field}` from `input`, percent-encoding the value. A field the caller did not
/// supply is an error rather than an empty string, because a URL missing a coordinate would still
/// return a plausible-looking answer about somewhere else.
pub fn fill(template: &str, input: &Value) -> Result<String, String> {
    let mut filled = template.to_owned();
    for field in placeholders(template) {
        let value = input
            .get(&field)
            .ok_or_else(|| format!("input does not carry {field:?}"))?;
        let plain = match value {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        let encoded: String =
            url::form_urlencoded::byte_serialize(plain.as_bytes()).collect::<String>();
        filled = filled.replace(
            &format!("{PLACEHOLDER_OPEN}{field}{PLACEHOLDER_CLOSE}"),
            &encoded.replace('+', "%20"),
        );
    }
    Ok(filled)
}

/// Follows a dotted path into a reply. Absent is `None`, and so is a path that runs into a scalar.
pub fn pick_value(body: &Value, path: &str) -> Option<Value> {
    let mut here = body;
    for segment in path.split('.') {
        here = here.get(segment)?;
    }
    Some(here.clone())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn schema_with(properties: serde_json::Value) -> serde_json::Value {
        json!({"type": "object", "properties": properties})
    }

    #[test]
    fn a_well_formed_set_is_parsed_in_full() {
        let raw = json!({
            "fan_out": 3,
            "take": 2,
            "sources": [
                {"name": "one", "url": "https://example.com/?q={city}", "pick": "current.temp"},
                {"name": "two", "url": "https://example.org/{city}", "pick": "temp"}
            ]
        });
        let set =
            parse_sources(&raw, &schema_with(json!({"city": {"type": "string"}}))).expect("parse");
        assert_eq!(set.fan_out, 3);
        assert_eq!(set.take, 2);
        assert_eq!(set.sources.len(), 2);
        assert_eq!(set.sources[0].pick, "current.temp");
    }

    #[test]
    fn a_placeholder_the_input_schema_does_not_declare_is_refused() {
        let raw = json!({
            "fan_out": 1, "take": 1,
            "sources": [{"name": "one", "url": "https://example.com/?q={region}", "pick": "temp"}]
        });
        let error = parse_sources(&raw, &schema_with(json!({"city": {"type": "string"}})))
            .expect_err("region is not an input field");
        assert!(error.contains("region"), "{error}");
    }

    #[test]
    fn take_larger_than_the_sources_is_refused() {
        let raw = json!({
            "fan_out": 1, "take": 4,
            "sources": [{"name": "one", "url": "https://example.com/", "pick": "temp"}]
        });
        let error = parse_sources(&raw, &schema_with(json!({}))).expect_err("take exceeds sources");
        assert!(error.contains("take"), "{error}");
    }

    #[test]
    fn an_empty_source_list_is_refused() {
        let raw = json!({"fan_out": 1, "take": 1, "sources": []});
        assert!(parse_sources(&raw, &schema_with(json!({}))).is_err());
    }

    #[test]
    fn a_filled_value_is_url_encoded() {
        let filled = fill(
            "https://example.com/?q={city}",
            &json!({"city": "San Francisco & Oakland"}),
        )
        .expect("fill");
        assert_eq!(
            filled,
            "https://example.com/?q=San%20Francisco%20%26%20Oakland"
        );
    }

    #[test]
    fn a_missing_input_value_names_the_field_it_wanted() {
        let error = fill("https://example.com/?q={city}", &json!({})).expect_err("no city");
        assert!(error.contains("city"), "{error}");
    }

    #[test]
    fn a_number_input_fills_without_its_json_quotes() {
        let filled = fill("https://example.com/?lat={lat}", &json!({"lat": 37.77})).expect("fill");
        assert_eq!(filled, "https://example.com/?lat=37.77");
    }

    #[test]
    fn a_dotted_path_reaches_a_nested_value() {
        let body = json!({"current": {"temperature_2m": 14.2}});
        assert_eq!(
            pick_value(&body, "current.temperature_2m"),
            Some(json!(14.2))
        );
    }

    #[test]
    fn a_path_that_is_not_there_is_none_rather_than_null() {
        let body = json!({"current": {"temperature_2m": 14.2}});
        assert_eq!(pick_value(&body, "current.humidity"), None);
    }
}
