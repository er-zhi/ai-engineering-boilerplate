// A tool whose behaviour is a registry row rather than Rust: several endpoints that answer the same
// question, raced through `crate::race`. Nothing here knows what any of them is about — the subject
// lives in the row's description, the same place a user-created tool's does.

use std::collections::HashSet;
use std::time::Duration;

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
    let mut seen_names = HashSet::new();
    for source in &set.sources {
        if !seen_names.insert(source.name.as_str()) {
            return Err(format!(
                "source name {:?} is used more than once",
                source.name
            ));
        }
        if source.pick.is_empty() {
            return Err(format!("source {:?} has an empty pick", source.name));
        }
        let fields = placeholders(&source.url)
            .map_err(|error| format!("source {:?}: {error}", source.name))?;
        for field in fields {
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

/// Scans out every `{field}` placeholder. An unclosed `{` is refused rather than silently
/// dropped — otherwise the literal brace reaches the HTTP layer unsubstituted, and only the
/// first caller who trips it finds out.
fn placeholders(template: &str) -> Result<Vec<String>, String> {
    let mut found = Vec::new();
    let mut rest = template;
    while let Some(start) = rest.find(PLACEHOLDER_OPEN) {
        let after = &rest[start + PLACEHOLDER_OPEN.len_utf8()..];
        match after.find(PLACEHOLDER_CLOSE) {
            Some(end) => {
                found.push(after[..end].to_owned());
                rest = &after[end + PLACEHOLDER_CLOSE.len_utf8()..];
            }
            None => return Err(format!("{template:?} has a {{ with no closing }}")),
        }
    }
    Ok(found)
}

/// Substitutes every `{field}` from `input`, percent-encoding the value. A field the caller did not
/// supply is an error rather than an empty string, because a URL missing a coordinate would still
/// return a plausible-looking answer about somewhere else.
pub fn fill(template: &str, input: &Value) -> Result<String, String> {
    let mut filled = template.to_owned();
    for field in placeholders(template)? {
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

const MAX_BODY_BYTES: usize = 256 * 1024;

/// Fills every source's URL, checks each one through `guard` **before** any request goes out, then
/// races them. Two answers are returned as two answers: agreeing sources confirm each other and
/// disagreeing ones are a fact the model has to see, so nothing here averages or picks between them.
pub async fn run<G, Fut>(
    client: &reqwest::Client,
    set: &SourceSet,
    input: &Value,
    per_source_timeout: Duration,
    guard: G,
) -> Result<Value, String>
where
    G: Fn(String) -> Fut + Send + Sync,
    Fut: std::future::Future<Output = Result<(), String>> + Send,
{
    let mut ready: Vec<(&Source, String)> = Vec::new();
    let mut refused: Vec<String> = Vec::new();
    for source in &set.sources {
        match fill(&source.url, input) {
            Ok(url) => match guard(url.clone()).await {
                Ok(()) => ready.push((source, url)),
                Err(reason) => refused.push(format!("{}: {reason}", source.name)),
            },
            Err(reason) => refused.push(format!("{}: {reason}", source.name)),
        }
    }
    // The name is borrowed from the row, never leaked: `run` is called on every execution of every
    // declarative tool, so a leak here grows with traffic.
    let ops: Vec<(&str, crate::race::OpFn<'_, Value>)> = ready
        .iter()
        .map(|(source, url)| {
            crate::race::op(source.name.as_str(), move || async move {
                ask(client, url, &source.pick).await
            })
        })
        .collect();

    let outcome = crate::race::race(&ops, set.fan_out, set.take, per_source_timeout).await;
    if outcome.taken.is_empty() {
        let mut reasons = refused;
        reasons.extend(outcome.failures);
        return Err(format!("every source failed — {}", reasons.join("; ")));
    }
    Ok(serde_json::json!({
        "values": outcome
            .taken
            .into_iter()
            .map(|(name, value)| serde_json::json!({"source": name, "value": value}))
            .collect::<Vec<_>>()
    }))
}

async fn ask(client: &reqwest::Client, url: &str, pick: &str) -> Result<Value, String> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|error| format!("request failed: {error}"))?;
    if !response.status().is_success() {
        return Err(format!("returned {}", response.status()));
    }
    let body = response
        .bytes()
        .await
        .map_err(|error| format!("body could not be read: {error}"))?;
    if body.len() > MAX_BODY_BYTES {
        return Err(format!("replied with more than {MAX_BODY_BYTES} bytes"));
    }
    let parsed: Value =
        serde_json::from_slice(&body).map_err(|error| format!("reply is not JSON: {error}"))?;
    pick_value(&parsed, pick).ok_or_else(|| format!("reply carries no {pick:?}"))
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
    fn an_unclosed_placeholder_is_refused() {
        let raw = json!({
            "fan_out": 1, "take": 1,
            "sources": [{"name": "one", "url": "https://example.com/{city", "pick": "temp"}]
        });
        let error = parse_sources(&raw, &schema_with(json!({"city": {"type": "string"}})))
            .expect_err("unclosed placeholder");
        assert!(error.contains("one"), "{error}");
    }

    #[test]
    fn duplicate_source_names_are_refused() {
        let raw = json!({
            "fan_out": 2, "take": 1,
            "sources": [
                {"name": "dup", "url": "https://example.com/a", "pick": "temp"},
                {"name": "dup", "url": "https://example.com/b", "pick": "temp"}
            ]
        });
        let error = parse_sources(&raw, &schema_with(json!({}))).expect_err("duplicate names");
        assert!(error.contains("dup"), "{error}");
    }

    #[test]
    fn an_empty_pick_is_refused() {
        let raw = json!({
            "fan_out": 1, "take": 1,
            "sources": [{"name": "one", "url": "https://example.com/", "pick": ""}]
        });
        let error = parse_sources(&raw, &schema_with(json!({}))).expect_err("empty pick");
        assert!(error.contains("pick"), "{error}");
    }

    #[test]
    fn fill_refuses_an_unclosed_placeholder_rather_than_diverging_from_parse_sources() {
        let error = fill("https://example.com/{city", &json!({"city": "x"})).expect_err("unclosed");
        assert!(error.contains('{'), "{error}");
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

    async fn serve(body: serde_json::Value) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("addr");
        let app = axum::Router::new().route(
            "/",
            axum::routing::get(move || {
                let body = body.clone();
                async move { axum::Json(body) }
            }),
        );
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{address}/")
    }

    const SOURCE_TIMEOUT: Duration = Duration::from_secs(5);

    fn set_of(sources: Vec<Source>, fan_out: usize, take: usize) -> SourceSet {
        SourceSet {
            fan_out,
            take,
            sources,
        }
    }

    fn source(name: &str, url: &str, pick: &str) -> Source {
        Source {
            name: name.to_owned(),
            url: url.to_owned(),
            pick: pick.to_owned(),
        }
    }

    #[tokio::test]
    async fn two_answers_come_back_with_the_sources_that_gave_them() {
        let one = serve(json!({"current": {"t": 14.2}})).await;
        let two = serve(json!({"current": {"t": 14.0}})).await;
        let set = set_of(
            vec![
                source("one", &one, "current.t"),
                source("two", &two, "current.t"),
            ],
            2,
            2,
        );
        let out = run(
            &reqwest::Client::new(),
            &set,
            &json!({}),
            SOURCE_TIMEOUT,
            |_| async { Ok(()) },
        )
        .await
        .expect("both answer");
        let values = out
            .get("values")
            .and_then(serde_json::Value::as_array)
            .expect("values");
        assert_eq!(values.len(), 2);
        let names: Vec<&str> = values
            .iter()
            .filter_map(|v| v.get("source")?.as_str())
            .collect();
        assert!(
            names.contains(&"one") && names.contains(&"two"),
            "{names:?}"
        );
    }

    #[tokio::test]
    async fn a_private_address_is_refused_before_any_request_goes_out() {
        let set = set_of(vec![source("local", "http://127.0.0.1:9/", "t")], 1, 1);
        let error = run(
            &reqwest::Client::new(),
            &set,
            &json!({}),
            SOURCE_TIMEOUT,
            |url: String| async move { crate::tools::web_fetch::ensure_public_url(&url).await },
        )
        .await
        .expect_err("loopback must be refused");
        assert!(error.contains("local"), "{error}");
    }

    #[tokio::test]
    async fn a_pick_that_finds_nothing_fails_that_source_not_the_whole_race() {
        let good = serve(json!({"current": {"t": 14.2}})).await;
        let thin = serve(json!({"current": {}})).await;
        let set = set_of(
            vec![
                source("thin", &thin, "current.t"),
                source("good", &good, "current.t"),
            ],
            2,
            1,
        );
        let out = run(
            &reqwest::Client::new(),
            &set,
            &json!({}),
            SOURCE_TIMEOUT,
            |_| async { Ok(()) },
        )
        .await
        .expect("the good source answers");
        let values = out
            .get("values")
            .and_then(serde_json::Value::as_array)
            .expect("values");
        assert_eq!(values.len(), 1);
        assert_eq!(
            values[0].get("source").and_then(serde_json::Value::as_str),
            Some("good")
        );
    }

    #[tokio::test]
    async fn every_source_failing_names_each_one() {
        let set = set_of(
            vec![
                source("one", "https://203.0.113.1/", "t"),
                source("two", "https://203.0.113.2/", "t"),
            ],
            2,
            1,
        );
        let error = run(
            &reqwest::Client::new(),
            &set,
            &json!({}),
            Duration::from_millis(200),
            |_| async { Ok(()) },
        )
        .await
        .expect_err("both are unroutable");
        assert!(error.contains("one") && error.contains("two"), "{error}");
    }
}
