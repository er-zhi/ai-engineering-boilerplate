// A tool whose behaviour is a registry row rather than Rust: several endpoints that answer the same
// question, raced through `crate::race`. Nothing here knows what any of them is about — the subject
// lives in the row's description, the same place a user-created tool's does.

use std::collections::HashSet;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

const PLACEHOLDER_OPEN: char = '{';
const PLACEHOLDER_CLOSE: char = '}';
/// What `pick_value` splits a path on, and therefore what a filled `pick` may not smuggle in.
const PATH_SEPARATOR: char = '.';

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
    /// Dotted path to the value in the source's JSON reply. Takes the same `{field}` placeholders a
    /// `url` does, because a reply's shape often depends on what was asked for — a source told to
    /// report one thing commonly keys the answer by the name of that thing. Filled verbatim rather
    /// than percent-encoded — this is a path into JSON, not a URL — except that a value carrying
    /// the path separator is refused, so an argument cannot walk deeper than the template says.
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
        for template in [&source.url, &source.pick] {
            let undeclared = placeholders(template)
                .map_err(|error| format!("source {:?}: {error}", source.name))?
                .into_iter()
                .find(|field| !declared.contains(field));
            // Naming the template, not just the field: a row using the same undeclared name in both
            // its url and its pick would otherwise get the identical sentence twice, and the
            // operator who fixed one reads it as "my edit did not take".
            if let Some(field) = undeclared {
                return Err(format!(
                    "source {:?} uses {{{field}}} in {template:?}, which input_schema does not declare",
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
    substitute(template, input, |_, plain| {
        let encoded: String =
            url::form_urlencoded::byte_serialize(plain.as_bytes()).collect::<String>();
        Ok(encoded.replace('+', "%20"))
    })
}

/// Substitutes every `{field}` from `input` verbatim. A `pick` is a path into a JSON reply rather
/// than a URL, so percent-encoding it would go looking for a key that is not there — a value with a
/// space in it would be sought as `%20` and simply not found.
///
/// A value carrying the path separator is refused instead. Percent-encoding was what kept a `url`'s
/// arguments from reaching into its structure, and a `pick` has structure too: unchecked, an
/// argument of `a.b` turns `rates.{to}` into a two-step walk the operator never wrote, letting
/// whoever supplies the arguments choose where in a third party's reply the answer is read from.
pub fn fill_path(template: &str, input: &Value) -> Result<String, String> {
    substitute(template, input, |field, plain| {
        if plain.contains(PATH_SEPARATOR) {
            return Err(format!(
                "{field:?} carries {PATH_SEPARATOR:?}, which would read deeper into the reply than the pick describes"
            ));
        }
        Ok(plain.to_owned())
    })
}

fn substitute(
    template: &str,
    input: &Value,
    render: impl Fn(&str, &str) -> Result<String, String>,
) -> Result<String, String> {
    let mut filled = template.to_owned();
    for field in placeholders(template)? {
        let value = input
            .get(&field)
            .ok_or_else(|| format!("input does not carry {field:?}"))?;
        let plain = match value {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        filled = filled.replace(
            &format!("{PLACEHOLDER_OPEN}{field}{PLACEHOLDER_CLOSE}"),
            &render(&field, &plain)?,
        );
    }
    Ok(filled)
}

/// Follows a dotted path into a reply. Absent is `None`, and so is a path that runs into a scalar.
///
/// A segment that is all digits indexes an array. Without it a whole class of real endpoints is
/// unreachable — returning a list of readings and expecting you to take the first is one of the
/// commonest shapes there is — and a row could describe the request but never the answer. An
/// all-digit segment against an object still reads it as a key first, so a reply that genuinely
/// uses numeric keys keeps working.
pub fn pick_value(body: &Value, path: &str) -> Option<Value> {
    let mut here = body;
    for segment in path.split(PATH_SEPARATOR) {
        here = match here.get(segment) {
            Some(found) => found,
            None => here.get(segment.parse::<usize>().ok()?)?,
        };
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
    let mut ready: Vec<(&Source, String, String)> = Vec::new();
    let mut refused: Vec<String> = Vec::new();
    for source in &set.sources {
        match fill(&source.url, input)
            .and_then(|url| fill_path(&source.pick, input).map(|pick| (url, pick)))
        {
            Ok((url, pick)) => match guard(url.clone()).await {
                Ok(()) => ready.push((source, url, pick)),
                Err(reason) => refused.push(format!("{}: {reason}", source.name)),
            },
            Err(reason) => refused.push(format!("{}: {reason}", source.name)),
        }
    }
    // The name is borrowed from the row, never leaked: `run` is called on every execution of every
    // declarative tool, so a leak here grows with traffic.
    let ops: Vec<(&str, crate::race::OpFn<'_, Value>)> = ready
        .iter()
        .map(|(source, url, pick)| {
            crate::race::op(source.name.as_str(), move || async move {
                ask(client, url, pick, per_source_timeout).await
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

async fn ask(
    client: &reqwest::Client,
    url: &str,
    pick: &str,
    timeout: Duration,
) -> Result<Value, String> {
    // `get_guarded` re-checks every redirect hop through the same SSRF guard `run` already applied
    // to this URL's first hop — a public source that answers 302 with a private `Location` must not
    // be able to walk past the guard just because the check only ever saw the URL it started with.
    let mut response = crate::tools::web_fetch::get_guarded(client, url, timeout).await?;
    if !response.status().is_success() {
        return Err(format!("returned {}", response.status()));
    }
    // Streamed rather than `response.bytes()`: checking the cap only after the whole body is
    // already buffered would let an endless or oversized reply keep allocating for the entire
    // per-source timeout before the bound ever fired.
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| format!("body could not be read: {error}"))?
    {
        body.extend_from_slice(&chunk);
        if body.len() > MAX_BODY_BYTES {
            return Err(format!("replied with more than {MAX_BODY_BYTES} bytes"));
        }
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
                {"name": "one", "url": "https://example.com/?q={place}", "pick": "reading.value"},
                {"name": "two", "url": "https://example.org/{place}", "pick": "value"}
            ]
        });
        let set =
            parse_sources(&raw, &schema_with(json!({"place": {"type": "string"}}))).expect("parse");
        assert_eq!(set.fan_out, 3);
        assert_eq!(set.take, 2);
        assert_eq!(set.sources.len(), 2);
        assert_eq!(set.sources[0].pick, "reading.value");
    }

    #[test]
    fn a_placeholder_the_input_schema_does_not_declare_is_refused() {
        let raw = json!({
            "fan_out": 1, "take": 1,
            "sources": [{"name": "one", "url": "https://example.com/?q={elsewhere}", "pick": "value"}]
        });
        let error = parse_sources(&raw, &schema_with(json!({"place": {"type": "string"}})))
            .expect_err("elsewhere is not an input field");
        assert!(error.contains("elsewhere"), "{error}");
    }

    #[test]
    fn a_pick_naming_a_field_the_input_schema_does_not_declare_is_refused() {
        let raw = json!({
            "fan_out": 1, "take": 1,
            "sources": [{"name": "one", "url": "https://example.com/", "pick": "rates.{elsewhere}"}]
        });
        let error = parse_sources(&raw, &schema_with(json!({"place": {"type": "string"}})))
            .expect_err("a pick gets the same check a url does");
        assert!(error.contains("elsewhere"), "{error}");
        // Naming the template is the point: a row with the same bad field in both would otherwise
        // give the operator the identical sentence after they fixed one of them.
        assert!(error.contains("rates.{elsewhere}"), "{error}");
    }

    /// The path separator is to a `pick` what percent-encoding kept out of a `url`: without this,
    /// whoever supplies the arguments chooses where in a third party's reply the answer is read.
    #[test]
    fn an_argument_carrying_the_path_separator_is_refused_by_a_pick_but_not_by_a_url() {
        let input = json!({"to": "a.b"});
        let error = fill_path("rates.{to}", &input).expect_err("a dot would walk a level deeper");
        assert!(error.contains("to"), "{error}");
        assert_eq!(
            fill("https://e.test/?q={to}", &input).expect("a url encodes it instead"),
            "https://e.test/?q=a.b"
        );
    }

    #[test]
    fn a_pick_is_filled_verbatim_while_a_url_is_encoded() {
        let input = json!({"name": "euro zone"});
        assert_eq!(
            fill("https://e.test/{name}", &input).expect("fill"),
            "https://e.test/euro%20zone"
        );
        assert_eq!(
            fill_path("rates.{name}", &input).expect("fill"),
            "rates.euro zone"
        );
    }

    #[test]
    fn take_larger_than_the_sources_is_refused() {
        let raw = json!({
            "fan_out": 1, "take": 4,
            "sources": [{"name": "one", "url": "https://example.com/", "pick": "value"}]
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
            "sources": [{"name": "one", "url": "https://example.com/{place", "pick": "value"}]
        });
        let error = parse_sources(&raw, &schema_with(json!({"place": {"type": "string"}})))
            .expect_err("unclosed placeholder");
        assert!(error.contains("one"), "{error}");
    }

    #[test]
    fn duplicate_source_names_are_refused() {
        let raw = json!({
            "fan_out": 2, "take": 1,
            "sources": [
                {"name": "dup", "url": "https://example.com/a", "pick": "value"},
                {"name": "dup", "url": "https://example.com/b", "pick": "value"}
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
        let error =
            fill("https://example.com/{place", &json!({"place": "x"})).expect_err("unclosed");
        assert!(error.contains('{'), "{error}");
    }

    #[test]
    fn a_filled_value_is_url_encoded() {
        let filled = fill(
            "https://example.com/?q={place}",
            &json!({"place": "Example Place & Other"}),
        )
        .expect("fill");
        assert_eq!(
            filled,
            "https://example.com/?q=Example%20Place%20%26%20Other"
        );
    }

    #[test]
    fn a_missing_input_value_names_the_field_it_wanted() {
        let error = fill("https://example.com/?q={place}", &json!({})).expect_err("no place");
        assert!(error.contains("place"), "{error}");
    }

    #[test]
    fn a_number_input_fills_without_its_json_quotes() {
        let filled =
            fill("https://example.com/?v={coord}", &json!({"coord": 37.77})).expect("fill");
        assert_eq!(filled, "https://example.com/?v=37.77");
    }

    #[test]
    fn a_dotted_path_reaches_a_nested_value() {
        let body = json!({"reading": {"value": 14.2}});
        assert_eq!(pick_value(&body, "reading.value"), Some(json!(14.2)));
    }

    #[test]
    fn a_path_that_is_not_there_is_none_rather_than_null() {
        let body = json!({"reading": {"value": 14.2}});
        assert_eq!(pick_value(&body, "reading.other"), None);
    }

    /// Returning a list of readings and expecting the caller to take the first is one of the
    /// commonest reply shapes there is; without this a row could describe such a request but never
    /// reach its answer.
    #[test]
    fn a_numeric_segment_indexes_an_array() {
        let body = json!({"readings": [{"value": 14.2}, {"value": 9.9}]});
        assert_eq!(pick_value(&body, "readings.0.value"), Some(json!(14.2)));
        assert_eq!(pick_value(&body, "readings.1.value"), Some(json!(9.9)));
        assert_eq!(pick_value(&body, "readings.2.value"), None);
    }

    /// An object is still read as an object first, so a reply that genuinely keys by a number is
    /// not silently reinterpreted as a list.
    #[test]
    fn a_numeric_key_on_an_object_still_wins_over_array_indexing() {
        let body = json!({"by_hour": {"0": {"value": 1.0}}});
        assert_eq!(pick_value(&body, "by_hour.0.value"), Some(json!(1.0)));
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

    /// A source told to report one named thing commonly keys the answer by that name, so the path to
    /// the value is not knowable until the argument is. Before `pick` took placeholders such a
    /// source could not be expressed as a row at all.
    #[tokio::test]
    async fn a_pick_whose_path_depends_on_the_argument_reaches_the_value() {
        let url = serve(json!({"rates": {"EUR": 0.87, "JPY": 147.0}})).await;
        let set = set_of(vec![source("one", &url, "rates.{to}")], 1, 1);
        let out = run(
            &reqwest::Client::new(),
            &set,
            &json!({"to": "EUR"}),
            SOURCE_TIMEOUT,
            |_| async { Ok(()) },
        )
        .await
        .expect("the source answers");
        assert_eq!(out["values"][0]["value"], json!(0.87));
    }

    /// The break-and-observe half of the rule above: run this against `fill` instead of `fill_path`
    /// and the key is sought as `euro%20zone`, which is not in the reply, so the source fails.
    #[tokio::test]
    async fn a_picked_key_carrying_a_space_is_not_percent_encoded() {
        let url = serve(json!({"rates": {"euro zone": 1.5}})).await;
        let set = set_of(vec![source("one", &url, "rates.{to}")], 1, 1);
        let out = run(
            &reqwest::Client::new(),
            &set,
            &json!({"to": "euro zone"}),
            SOURCE_TIMEOUT,
            |_| async { Ok(()) },
        )
        .await
        .expect("the source answers");
        assert_eq!(out["values"][0]["value"], json!(1.5));
    }

    /// `parse_sources` proves a pick's fields are *declared*; nothing makes them *required*, so a
    /// caller can still omit one. That source is named as refused and no request goes out — the
    /// guard is the only thing that ever sees a URL, and it is never reached.
    #[tokio::test]
    async fn a_pick_whose_field_the_caller_omitted_refuses_that_source_without_asking_anyone() {
        let set = set_of(vec![source("one", "https://e.test/", "rates.{to}")], 1, 1);
        let error = run(
            &reqwest::Client::new(),
            &set,
            &json!({}),
            SOURCE_TIMEOUT,
            |_| async { panic!("no request may be prepared for a source that cannot be filled") },
        )
        .await
        .expect_err("the only source cannot be filled");
        assert!(error.contains("one") && error.contains("\"to\""), "{error}");
    }

    #[tokio::test]
    async fn a_pick_argument_carrying_the_path_separator_refuses_that_source_through_run() {
        let set = set_of(vec![source("one", "https://e.test/", "rates.{to}")], 1, 1);
        let error = run(
            &reqwest::Client::new(),
            &set,
            &json!({"to": "a.b"}),
            SOURCE_TIMEOUT,
            |_| async { panic!("a pick that walks too deep is refused before any request") },
        )
        .await
        .expect_err("a dot in the argument is refused");
        assert!(error.contains("one") && error.contains('.'), "{error}");
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

    #[tokio::test]
    async fn an_endless_body_is_abandoned_once_it_passes_the_cap() {
        let app = axum::Router::new().route(
            "/",
            axum::routing::get(|| async {
                axum::body::Body::from_stream(futures::stream::repeat_with(|| {
                    Ok::<_, std::io::Error>(vec![b'x'; 64 * 1024])
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let client = reqwest::Client::new();

        let error = tokio::time::timeout(
            Duration::from_secs(30),
            ask(&client, &format!("http://{address}/"), "t", SOURCE_TIMEOUT),
        )
        .await
        .expect("a body that never ends must not be buffered whole")
        .unwrap_err();

        assert!(error.contains(&MAX_BODY_BYTES.to_string()), "{error}");
    }
}
