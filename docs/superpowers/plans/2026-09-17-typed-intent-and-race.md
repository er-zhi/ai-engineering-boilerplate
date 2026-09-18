# Typed Intent and the `race` Primitive — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the platform a generic `race` primitive that starts a fan-out of operations, takes the first N successes and cancels the rest mid-flight; put weather / stocks / fx on it as registry rows rather than Rust modules; and make `SystemOneService.Decide` the first thing a user turn hits, so an unclear message is answered in one round trip instead of starting an agent.

**Architecture:** `race` is a generic combinator in `services/tool`, taking operation *factories* so a retry or a reserve start can build a fresh attempt. `web_fetch` is rewritten on top of it. A new nullable `sources` jsonb column on `tool.tools` turns one row into a declarative racing HTTP tool, and `weather` / `stock_quote` / `fx_rate` become seeded rows — no subject enters Rust or any prompt. In `services/chat`, `classifier.rs` becomes `intent.rs`: one `Decide` call carrying three questions (`actionable`, `route`, `separate_themes`), with `Complete` reached for only on the multi-theme branch, and a new session-level `CLARIFICATION_NEEDED` event when there is nothing to act on.

**Tech Stack:** Rust 2024, SeaORM 2.0 (entity-first schema-sync), Connect RPC (`connectrpc` + `buffa`), `futures::stream::FuturesUnordered`, Postgres, testcontainers, `cargo-nextest`.

**Spec:** `docs/superpowers/specs/2026-09-17-typed-intent-and-race-design.md` — read it before starting.

## Global Constraints

- Cargo is not on the default PATH: prefix every shell command with `export PATH="$HOME/.cargo/bin:$PATH"`.
- Strict lints on every new file: `unsafe_code = "forbid"`, clippy `all = deny`, `unwrap_used = "deny"`, `too_many_lines = "deny"`, `cognitive_complexity = "deny"`, `too_many_arguments = "deny"`.
- No `async_trait`: ports return `impl Future<Output = ...> + Send` in return position.
- Entity-first SeaORM. No hand-written `.sql` migrations; schema-sync creates the new column.
- The review gates in `.agents/skills/code-review/` are **not edited by this work**. In particular `gate-architecture`'s "Capabilities, Not Topics" stands: no subject name may appear in Rust code or in any prompt string.
- Full check suite before pushing (`docs/development.md`):
  ```bash
  export PATH="$HOME/.cargo/bin:$PATH"
  cargo fmt --all -- --check
  cargo clippy --workspace --all-targets --all-features -- -D warnings
  cargo nextest run --workspace
  cargo deny check
  cargo machete
  ```
- Docker must be running for database tests.
- **buffa gotcha:** a `google.protobuf.Value` field (`DecideRequest.state`, `Question.instructions`) has no public constructor from `serde_json::Value`. The only idiom in this workspace is to deserialize the whole message from JSON and then set the plain fields, exactly as `services/llm-router/src/test_system_one.rs:106-111` does:
  ```rust
  let mut request: DecideRequest = serde_json::from_value(json!({"state": state}))?;
  request.questions = questions;
  ```

---

### Task 1: The `race` primitive

**Files:**
- Create: `services/tool/src/race.rs`
- Modify: `services/tool/src/lib.rs` (add `pub mod race;`)
- Test: inline `#[cfg(test)] mod tests` in `services/tool/src/race.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: `tool::race::{race, Outcome, OpFn, OpFuture, op}` —
  `pub async fn race<'a, T>(ops: &'a [(&'a str, OpFn<'a, T>)], fan_out: usize, take: usize, per_op_timeout: Duration) -> Outcome<'a, T>`,
  `pub struct Outcome<'a, T> { pub taken: Vec<(&'a str, T)>, pub failures: Vec<String> }`,
  `pub fn op<'a, T, F, Fut>(name: &'a str, f: F) -> (&'a str, OpFn<'a, T>)`.

> **The name is borrowed, never `&'static str`.** An operation's name comes from a URL or a
> registry row at runtime. Naming it `&'static str` forces callers to `Box::leak` on **every**
> execution, which is an unbounded leak, not a bounded one. The slice already has a lifetime;
> the name rides it.

- [ ] **Step 1: Write the failing tests**

Add to `services/tool/src/race.rs`:

```rust
#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    const QUICK: Duration = Duration::from_secs(5);

    /// Counts cancellations only. `resolved` is set on the success path, so a future that ran to
    /// completion is not confused with one the race cut loose — without that flag the counter would
    /// tick either way and the test would prove nothing.
    struct Tracked {
        cancelled: Arc<AtomicUsize>,
        resolved: bool,
    }

    impl Drop for Tracked {
        fn drop(&mut self) {
            if !self.resolved {
                self.cancelled.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    /// Every race in these tests is wrapped in this: a test that asserts the race *stops early*
    /// must fail when it does not, rather than pass slowly after the slow operation finishes.
    const MUST_FINISH_WITHIN: Duration = Duration::from_millis(500);

    fn answering<'a>(name: &'a str, after: Duration) -> (&'a str, OpFn<'a, &'a str>) {
        op(name, move || async move {
            tokio::time::sleep(after).await;
            Ok(name)
        })
    }

    fn failing<'a>(name: &'a str) -> (&'a str, OpFn<'a, &'a str>) {
        op(name, move || async move { Err("refused".to_owned()) })
    }

    #[tokio::test]
    async fn takes_the_quorum_and_stops_without_waiting_for_the_rest() {
        let ops = [
            answering("a", Duration::from_millis(5)),
            answering("b", Duration::from_millis(10)),
            answering("c", Duration::from_secs(30)),
        ];
        let outcome = tokio::time::timeout(MUST_FINISH_WITHIN, race(&ops, 3, 2, QUICK))
            .await
            .expect("the quorum was reached, so the race must not wait for c");
        assert_eq!(
            outcome.taken.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
            vec!["a", "b"]
        );
    }

    #[tokio::test]
    async fn the_losers_are_cancelled_rather_than_awaited() {
        let cancelled = Arc::new(AtomicUsize::new(0));
        let slow = cancelled.clone();
        let ops = [
            answering("fast", Duration::from_millis(1)),
            op("slow", move || {
                let mut guard = Tracked {
                    cancelled: slow.clone(),
                    resolved: false,
                };
                async move {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    guard.resolved = true;
                    Ok("slow")
                }
            }),
        ];
        let outcome = tokio::time::timeout(MUST_FINISH_WITHIN, race(&ops, 2, 1, QUICK))
            .await
            .expect("the race must return on the first success");
        assert_eq!(outcome.taken.len(), 1);
        assert_eq!(
            cancelled.load(Ordering::SeqCst),
            1,
            "the slow op resolved or was awaited instead of being dropped"
        );
    }

    #[tokio::test]
    async fn a_failure_starts_the_next_reserve() {
        let ops = [
            failing("one"),
            failing("two"),
            failing("three"),
            answering("four", Duration::from_millis(1)),
            answering("five", Duration::from_millis(1)),
        ];
        let outcome = race(&ops, 3, 1, QUICK).await;
        assert_eq!(outcome.taken.len(), 1, "the reserve never started");
        assert_eq!(outcome.failures.len(), 3);
    }

    #[tokio::test]
    async fn every_failure_is_named() {
        let ops = [failing("one"), failing("two")];
        let outcome = race(&ops, 2, 1, QUICK).await;
        assert!(outcome.taken.is_empty());
        assert!(outcome.failures.iter().any(|f| f.starts_with("one: ")), "{:?}", outcome.failures);
        assert!(outcome.failures.iter().any(|f| f.starts_with("two: ")), "{:?}", outcome.failures);
    }

    #[tokio::test]
    async fn an_operation_past_its_timeout_fails_instead_of_hanging_the_race() {
        let ops = [answering("slow", Duration::from_secs(30)), answering("quick", Duration::from_millis(1))];
        let outcome = race(&ops, 2, 2, Duration::from_millis(50)).await;
        assert_eq!(outcome.taken.len(), 1);
        assert!(outcome.failures[0].contains("timed out"), "{:?}", outcome.failures);
    }

    #[tokio::test]
    async fn no_operations_is_a_named_failure_not_a_panic() {
        let ops: [(&str, OpFn<'_, &str>); 0] = [];
        let outcome = race(&ops, 3, 1, QUICK).await;
        assert!(outcome.taken.is_empty());
        assert_eq!(outcome.failures, vec!["no operations to race".to_owned()]);
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo nextest run -p tool race::
```
Expected: FAIL to compile — `race`, `Outcome`, `op`, `OpFn` are not defined.

- [ ] **Step 3: Write the implementation**

Put above the test module in `services/tool/src/race.rs`:

```rust
// race: start a fan-out of independent operations, keep the first `take` successes and cancel the
// rest where they stand. It knows nothing about HTTP, URLs or any subject — the operations are the
// caller's, and the only thing here is the shape of "several ways to get the same thing".
//
// Operations are `Fn() -> Future` factories rather than plain futures on purpose: a future that has
// already resolved — or been cancelled at its timeout — cannot be polled again, so a reserve start
// needs a fresh one. Boxing them also lets closures of different concrete types share one slice.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use futures::StreamExt;
use futures::stream::FuturesUnordered;

pub type OpFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, String>> + Send + 'a>>;
pub type OpFn<'a, T> = Box<dyn Fn() -> OpFuture<'a, T> + Send + Sync + 'a>;

/// Boxes an operation factory so callers can put differently-typed closures in one slice.
pub fn op<'a, T, F, Fut>(name: &'a str, f: F) -> (&'a str, OpFn<'a, T>)
where
    F: Fn() -> Fut + Send + Sync + 'a,
    Fut: Future<Output = Result<T, String>> + Send + 'a,
{
    (name, Box::new(move || Box::pin(f())))
}

/// What a race produced. Not a `Result`: a partial answer is a normal outcome, and only the caller
/// knows whether fewer than `take` successes is enough for what it is doing.
#[derive(Debug)]
pub struct Outcome<'a, T> {
    /// Up to `take` successes, in the order they arrived, each with the name of the operation that
    /// produced it.
    pub taken: Vec<(&'a str, T)>,
    /// One `"name: reason"` per operation that was started and did not succeed. Operations still in
    /// flight when the quorum was reached are cancelled and appear nowhere: they have no verdict.
    pub failures: Vec<String>,
}

/// Runs at most `fan_out` operations at a time, each capped at `per_op_timeout`, until `take` of
/// them have succeeded or the list is exhausted. A finished operation immediately makes room for the
/// next one in the slice, so the width stays full. Reaching the quorum drops the whole set of
/// in-flight futures, which cancels them where they stand rather than waiting them out.
pub async fn race<'a, T>(
    ops: &'a [(&'a str, OpFn<'a, T>)],
    fan_out: usize,
    take: usize,
    per_op_timeout: Duration,
) -> Outcome<'a, T> {
    if ops.is_empty() {
        return Outcome {
            taken: Vec::new(),
            failures: vec!["no operations to race".to_owned()],
        };
    }
    let width = fan_out.max(1);
    let quorum = take.max(1);
    let mut next = 0;
    let mut running = FuturesUnordered::new();
    let mut outcome = Outcome {
        taken: Vec::new(),
        failures: Vec::new(),
    };

    while next < ops.len() && running.len() < width {
        running.push(attempt(&ops[next], per_op_timeout));
        next += 1;
    }
    while let Some(result) = running.next().await {
        match result {
            Ok(taken) => {
                outcome.taken.push(taken);
                if outcome.taken.len() >= quorum {
                    break;
                }
            }
            Err(failure) => outcome.failures.push(failure),
        }
        if next < ops.len() {
            running.push(attempt(&ops[next], per_op_timeout));
            next += 1;
        }
    }
    outcome
}

async fn attempt<'a, T>(
    (name, factory): &'a (&'a str, OpFn<'a, T>),
    per_op_timeout: Duration,
) -> Result<(&'a str, T), String> {
    match tokio::time::timeout(per_op_timeout, factory()).await {
        Ok(Ok(value)) => Ok((name, value)),
        Ok(Err(reason)) => Err(format!("{name}: {reason}")),
        // `{:?}` rather than `as_secs()`: a 50 ms cap printed as "0s" reads like a bug report
        // about the wrong thing.
        Err(_) => Err(format!("{name}: timed out after {per_op_timeout:?}")),
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo nextest run -p tool race::
cargo clippy -p tool --all-targets -- -D warnings
```
Expected: 6 passed, clippy clean.

- [ ] **Step 5: Commit**

```bash
git add services/tool/src/race.rs services/tool/src/lib.rs
git commit -m "tool: race starts a fan-out, takes the first N and cancels the rest"
```

---

### Task 2: `web_fetch` on top of `race`

**Files:**
- Modify: `services/tool/src/tools/web_fetch.rs:166-192` (`fetch_richest`)
- Test: inline tests in the same file

**Interfaces:**
- Consumes: `tool::race::{race, op, OpFn}` from Task 1.
- Produces: `fetch_richest` keeps its signature `async fn fetch_richest(client: &reqwest::Client, urls: &[String], timeout: Duration) -> Result<Extracted, String>`.

- [ ] **Step 1: Write the failing test**

Add to the existing `mod tests` in `services/tool/src/tools/web_fetch.rs`:

```rust
    #[tokio::test]
    async fn fetch_richest_names_every_url_that_failed() {
        let client = reqwest::Client::new();
        let urls = vec![
            "http://127.0.0.1:1/a".to_owned(),
            "http://127.0.0.1:1/b".to_owned(),
        ];
        let error = fetch_richest(&client, &urls, TEST_TIMEOUT)
            .await
            .expect_err("both urls are unreachable");
        assert!(error.contains("/a"), "{error}");
        assert!(error.contains("/b"), "{error}");
    }
```

- [ ] **Step 2: Run it to see it pass against the old code, then rewrite**

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo nextest run -p tool web_fetch::
```
Expected: PASS — this test pins the behaviour that must survive the rewrite. Keep it green through steps 3-4.

- [ ] **Step 3: Rewrite `fetch_richest` over `race`**

Replace the body of `fetch_richest` (and delete the now-unused `FuturesUnordered` / `StreamExt` imports if nothing else in the file uses them):

```rust
pub async fn fetch_richest(
    client: &reqwest::Client,
    urls: &[String],
    timeout: Duration,
) -> Result<Extracted, String> {
    // The candidates are raced, but the winner is the fullest page rather than the fastest one:
    // whoever answers first is decided by network latency, which has nothing to do with whether the
    // page holds the answer. The race decides *when to stop waiting*; the comparison decides *what
    // to return*, and it needs something to compare against.
    let ops: Vec<(&str, crate::race::OpFn<'_, Extracted>)> = urls
        .iter()
        .map(|url| crate::race::op(url.as_str(), move || fetch(client, url, timeout)))
        .collect();
    let outcome = crate::race::race(&ops, urls.len(), ANSWERS_BEFORE_CHOOSING, timeout).await;
    let failures = outcome.failures.join("; ");
    outcome
        .taken
        .into_iter()
        .map(|(_, page)| page)
        .max_by_key(|page| page.main_text.len())
        .ok_or_else(|| format!("every url failed — {failures}"))
}
```

> The operation name is `url.as_str()`, borrowed from the caller's slice. It must never be
> `Box::leak`ed: `fetch_richest` runs on every `web_fetch` execution, so leaking a name per URL
> per call is an unbounded leak, not one bounded by `MAX_URLS`.

- [ ] **Step 4: Run the whole web_fetch suite**

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo nextest run -p tool web_fetch::
cargo clippy -p tool --all-targets -- -D warnings
```
Expected: all previously passing web_fetch tests still pass, including the new one.

- [ ] **Step 5: Commit**

```bash
git add services/tool/src/tools/web_fetch.rs
git commit -m "tool: web_fetch races its candidates through the shared primitive"
```

---

### Task 3: The `sources` column and its parsed form

**Files:**
- Modify: `services/tool/src/entity/tool.rs` (add the column)
- Create: `services/tool/src/tools/declarative.rs` (config type + parsing/validation only; the executor lands in Task 4)
- Modify: `services/tool/src/tools/mod.rs` (add `pub mod declarative;`)
- Test: inline tests in `services/tool/src/tools/declarative.rs`

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces: `tool::tools::declarative::{SourceSet, Source, parse_sources, fill}` —
  `pub struct SourceSet { pub fan_out: usize, pub take: usize, pub sources: Vec<Source> }`,
  `pub struct Source { pub name: String, pub url: String, pub pick: String }`,
  `pub fn parse_sources(raw: &serde_json::Value, input_schema: &serde_json::Value) -> Result<SourceSet, String>`,
  `pub fn fill(template: &str, input: &serde_json::Value) -> Result<String, String>`,
  `pub fn pick_value(body: &serde_json::Value, path: &str) -> Option<serde_json::Value>`.
- Entity gains `pub sources: Option<Json>`.

- [ ] **Step 1: Write the failing tests**

Create `services/tool/src/tools/declarative.rs` with only this test module for now:

```rust
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
        let set = parse_sources(&raw, &schema_with(json!({"city": {"type": "string"}}))).expect("parse");
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
        assert_eq!(filled, "https://example.com/?q=San%20Francisco%20%26%20Oakland");
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
        assert_eq!(pick_value(&body, "current.temperature_2m"), Some(json!(14.2)));
    }

    #[test]
    fn a_path_that_is_not_there_is_none_rather_than_null() {
        let body = json!({"current": {"temperature_2m": 14.2}});
        assert_eq!(pick_value(&body, "current.humidity"), None);
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo nextest run -p tool declarative::
```
Expected: FAIL to compile — `parse_sources`, `fill`, `pick_value` are not defined.

- [ ] **Step 3: Write the implementation**

Put above the test module in `services/tool/src/tools/declarative.rs`:

```rust
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
```

- [ ] **Step 4: Add the entity column**

In `services/tool/src/entity/tool.rs`, inside `struct Model`, after `output_schema`:

```rust
    /// Set on a declarative tool: the endpoints it races and how to read them. `None` means this
    /// row's behaviour is Rust, dispatched by slug.
    #[sea_orm(column_type = "JsonBinary", nullable)]
    pub sources: Option<Json>,
```

- [ ] **Step 5: Run the tests and the schema sync**

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo nextest run -p tool declarative::
cargo nextest run -p tool entity:: service::
cargo clippy -p tool --all-targets -- -D warnings
```
Expected: the 9 new declarative tests pass; existing tool tests still pass (schema-sync adds the nullable column to the fresh test databases).

- [ ] **Step 6: Commit**

```bash
git add services/tool/src/tools/declarative.rs services/tool/src/tools/mod.rs services/tool/src/entity/tool.rs
git commit -m "tool: a tools row can declare the endpoints it races"
```

---

### Task 4: The declarative executor

**Files:**
- Modify: `services/tool/src/tools/declarative.rs` (add `run`)
- Modify: `services/tool/src/service.rs:314-317` (`execute` — branch on `tool.sources` before `run_system_tool`)
- Test: inline tests in `services/tool/src/tools/declarative.rs` using an `axum` server on loopback, following the pattern in `services/tool/src/service.rs`'s `serve_llm`

**Interfaces:**
- Consumes: `tool::race::{race, op, OpFn}` (Task 1); `parse_sources`, `fill`, `pick_value`, `SourceSet` (Task 3); `tool::tools::web_fetch::ensure_public_url`.
- Produces, final signature — the guard is a parameter so tests can reach loopback without weakening `ensure_public_url`:
  ```rust
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
  ```
  returning `{"values": [{"source": "...", "value": ...}]}`.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `services/tool/src/tools/declarative.rs`:

```rust
    use std::time::Duration;

    async fn serve(body: serde_json::Value) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("addr");
        let app = axum::Router::new().route("/", axum::routing::get(move || {
            let body = body.clone();
            async move { axum::Json(body) }
        }));
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{address}/")
    }

    const SOURCE_TIMEOUT: Duration = Duration::from_secs(5);

    fn set_of(sources: Vec<Source>, fan_out: usize, take: usize) -> SourceSet {
        SourceSet { fan_out, take, sources }
    }

    fn source(name: &str, url: &str, pick: &str) -> Source {
        Source { name: name.to_owned(), url: url.to_owned(), pick: pick.to_owned() }
    }

    #[tokio::test]
    async fn two_answers_come_back_with_the_sources_that_gave_them() {
        let one = serve(json!({"current": {"t": 14.2}})).await;
        let two = serve(json!({"current": {"t": 14.0}})).await;
        let set = set_of(
            vec![source("one", &one, "current.t"), source("two", &two, "current.t")],
            2,
            2,
        );
        let out = run(&reqwest::Client::new(), &set, &json!({}), SOURCE_TIMEOUT)
            .await
            .expect("both answer");
        let values = out.get("values").and_then(serde_json::Value::as_array).expect("values");
        assert_eq!(values.len(), 2);
        let names: Vec<&str> = values.iter().filter_map(|v| v.get("source")?.as_str()).collect();
        assert!(names.contains(&"one") && names.contains(&"two"), "{names:?}");
    }

    #[tokio::test]
    async fn a_private_address_is_refused_before_any_request_goes_out() {
        let set = set_of(vec![source("local", "http://127.0.0.1:9/", "t")], 1, 1);
        let error = run(&reqwest::Client::new(), &set, &json!({}), SOURCE_TIMEOUT)
            .await
            .expect_err("loopback must be refused");
        assert!(error.contains("local"), "{error}");
    }

    #[tokio::test]
    async fn a_pick_that_finds_nothing_fails_that_source_not_the_whole_race() {
        let good = serve(json!({"current": {"t": 14.2}})).await;
        let thin = serve(json!({"current": {}})).await;
        let set = set_of(
            vec![source("thin", &thin, "current.t"), source("good", &good, "current.t")],
            2,
            1,
        );
        let out = run(&reqwest::Client::new(), &set, &json!({}), SOURCE_TIMEOUT)
            .await
            .expect("the good source answers");
        let values = out.get("values").and_then(serde_json::Value::as_array).expect("values");
        assert_eq!(values.len(), 1);
        assert_eq!(values[0].get("source").and_then(serde_json::Value::as_str), Some("good"));
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
        let error = run(&reqwest::Client::new(), &set, &json!({}), Duration::from_millis(200))
            .await
            .expect_err("both are unroutable");
        assert!(error.contains("one") && error.contains("two"), "{error}");
    }
```

> The loopback test relies on `ensure_public_url` rejecting `127.0.0.1`, which it already does (`web_fetch.rs`'s `rejects_hosts_inside_the_network_tool_runs_in`). The two servers in the other tests are reached **through `run`'s own filled URLs**, so `run` must apply `ensure_public_url` — meaning those two tests would fail against loopback too. Resolve this by having `run` take the guard as a parameter: `guard: impl Fn(&str) -> F`. Concretely, use the signature below and pass `web_fetch::ensure_public_url` from `service.rs` while the tests pass a permissive closure. Do not weaken `ensure_public_url`.

Corrected signature to implement:

```rust
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
```

Update the four tests above to pass `|_| async { Ok(()) }` as `guard`, except `a_private_address_is_refused_before_any_request_goes_out`, which passes `|url: String| async move { crate::tools::web_fetch::ensure_public_url(&url).await }`.

- [ ] **Step 2: Run the tests to verify they fail**

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo nextest run -p tool declarative::
```
Expected: FAIL to compile — `run` is not defined.

- [ ] **Step 3: Write the implementation**

Add to `services/tool/src/tools/declarative.rs`:

```rust
use std::time::Duration;

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
            crate::race::op(source.name.as_str(), move || {
                async move { ask(client, url, &source.pick).await }
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
```

- [ ] **Step 4: Branch `execute` onto it**

In `services/tool/src/service.rs`, replace the last two lines of `execute`:

```rust
        let input: Value = serde_json::from_str(input_json)
            .map_err(|e| ToolError::InvalidRequest(e.to_string()))?;
        let timeout = registered_timeout(tool.timeout_seconds);
        if let Some(raw) = tool.sources.clone() {
            return self.run_declarative(&tool, &raw, &input, timeout).await;
        }
        self.run_system_tool(slug, &input, timeout).await
    }

    /// A row that declares its sources runs through one shared executor: its slug is a name, not a
    /// branch, and this service holds no knowledge of what any of them is about.
    async fn run_declarative(
        &self,
        tool: &crate::entity::tool::Model,
        raw: &Value,
        input: &Value,
        timeout: Duration,
    ) -> Result<ExecuteOutcome, ToolError> {
        let set = match crate::tools::declarative::parse_sources(raw, &tool.input_schema) {
            Ok(set) => set,
            Err(problem) => return Ok(ExecuteOutcome::NotExecutable(problem)),
        };
        Ok(
            match crate::tools::declarative::run(&self.http, &set, input, timeout, |url| async move {
                crate::tools::web_fetch::ensure_public_url(&url).await
            })
            .await
            {
                Ok(value) => ExecuteOutcome::Ok(value),
                Err(error) => ExecuteOutcome::Error(error),
            },
        )
    }
```

- [ ] **Step 5: Run the tests**

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo nextest run -p tool
cargo clippy -p tool --all-targets -- -D warnings
```
Expected: all pass.

- [ ] **Step 6: Commit**

```bash
git add services/tool/src/tools/declarative.rs services/tool/src/service.rs
git commit -m "tool: one executor runs every declarative row, whatever it is about"
```

---

### Task 5: Seed the three declarative rows

**Files:**
- Modify: `services/tool/src/slugs.rs`
- Modify: `services/tool/src/main.rs:208-240` (`system_tool_definitions`, `SystemTool`, `seeding_already_finished`, `create_and_activate_system_tool`, `activate_without_llm_review`)
- Test: inline tests in `services/tool/src/main.rs`

**Interfaces:**
- Consumes: `parse_sources` (Task 3).
- Produces: `slugs::{WEATHER, STOCK_QUOTE, FX_RATE}` and `RESERVED_FOR_SYSTEM_TOOLS: [&str; 7]`; `SystemTool` gains `sources: Option<serde_json::Value>`.

- [ ] **Step 1: Recover the verified endpoints**

```bash
git show d5a8d00:services/tool/src/tools/weather.rs > /tmp/weather-reference.rs
git show d5a8d00:services/tool/src/tools/fx.rs > /tmp/fx-reference.rs
git show d5a8d00:services/tool/src/tools/stocks.rs > /tmp/stocks-reference.rs
```

Read each for its keyless endpoints, the JSON path to the value, and the unit. Then **verify each endpoint with a live request** before putting it in the seed — these are free public APIs and they move:

```bash
curl -s 'https://api.open-meteo.com/v1/forecast?latitude=37.77&longitude=-122.42&current=temperature_2m' | head -c 400
```

- [ ] **Step 2: Write the failing test**

Add to `mod tests` in `services/tool/src/main.rs`:

```rust
    #[test]
    fn every_declarative_definition_parses_against_its_own_input_schema() {
        for definition in system_tool_definitions() {
            let Some(raw) = definition.sources.as_ref() else {
                continue;
            };
            tool::tools::declarative::parse_sources(raw, &definition.input_schema)
                .unwrap_or_else(|error| panic!("{}: {error}", definition.slug));
        }
    }

    #[test]
    fn every_seeded_slug_is_reserved() {
        for definition in system_tool_definitions() {
            assert!(
                tool::slugs::RESERVED_FOR_SYSTEM_TOOLS.contains(&definition.slug),
                "{} is seeded but not reserved",
                definition.slug
            );
        }
    }
```

- [ ] **Step 3: Run it to verify it fails**

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo nextest run -p tool --bin tool
```
Expected: FAIL to compile — `SystemTool` has no `sources` field.

- [ ] **Step 4: Implement**

In `services/tool/src/slugs.rs`:

```rust
pub const WEATHER: &str = "weather";
pub const STOCK_QUOTE: &str = "stock_quote";
pub const FX_RATE: &str = "fx_rate";

pub const RESERVED_FOR_SYSTEM_TOOLS: [&str; 7] = [
    WEB_SEARCH,
    WEB_FETCH,
    KB_SEARCH,
    KB_READ_DOCUMENT,
    WEATHER,
    STOCK_QUOTE,
    FX_RATE,
];
```

**`args.rs` gains nothing.** A Rust arg type there earns its place because `run_system_tool` parses
into it, and the derived `JsonSchema` and the reader are then one declaration. A declarative row has
no Rust reader — `fill` reads the raw input — so a type would be a second declaration enforcing
nothing: `{"lat": "x"}` would still fill the URL. Write each row's `input_schema` as a JSON literal
beside its `sources`, where the `parse_sources` placeholder check can see it.

In `services/tool/src/main.rs`, add `sources: Option<serde_json::Value>` to `SystemTool`, include it in `seeding_already_finished`'s comparison (`row.sources == definition.sources`), set it on the `ActiveModel` in `create_and_activate_system_tool` and `activate_without_llm_review`, change the return type to `[SystemTool; 7]`, set `sources: None` on the four existing entries, and append (filling the second and third source of each from Step 1's verified endpoints):

```rust
        SystemTool {
            slug: slugs::WEATHER,
            name: "Current temperature at a coordinate",
            description: "Returns the current temperature in degrees Celsius at a latitude and longitude, from several independent providers at once.",
            risk: Risk::ReadOnly,
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["lat", "lon"],
                "additionalProperties": false,
                "properties": {
                    "lat": {"type": "number", "description": "Latitude in decimal degrees."},
                    "lon": {"type": "number", "description": "Longitude in decimal degrees."}
                }
            }),
            sources: Some(serde_json::json!({
                "fan_out": 3,
                "take": 2,
                "sources": [
                    {"name": "open-meteo",
                     "url": "https://api.open-meteo.com/v1/forecast?latitude={lat}&longitude={lon}&current=temperature_2m",
                     "pick": "current.temperature_2m"}
                ]
            })),
        },
```

…and the equivalent `stock_quote` (`{"symbol": {"type": "string"}}`) and `fx_rate`
(`{"base": …, "quote": …}`) entries. Each needs **at least 3 sources** for `fan_out: 3, take: 2` to
be meaningful; `parse_sources` refuses `take` above the source count, so the test in Step 2 catches
a short list.

Also call `parse_sources` inside `create_and_activate_system_tool` / `activate_without_llm_review`
and log-and-skip a row it rejects, so a bad seed never reaches the registry.

Modify `services/tool/src/args.rs`: nothing. See the note above.

> **Gate check before committing:** grep your diff for subject words outside a seed literal's `description`, `name` and `url`. `rg -n 'weather|stock|fx|temperature' services/tool/src --glob '!main.rs'` must return nothing but `slugs.rs`. No prompt string anywhere gains a subject.

- [ ] **Step 5: Run the tests**

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo nextest run -p tool
cargo clippy -p tool --all-targets -- -D warnings
```
Expected: all pass.

- [ ] **Step 6: Commit**

```bash
git add services/tool/src/slugs.rs services/tool/src/main.rs
git commit -m "tool: three declarative rows, each racing three open APIs for two answers"
```

---

### Task 6: `validate_tool` decides instead of completing

**Files:**
- Modify: `services/tool/src/service.rs:6` (imports), `:133-160` (add the System One client), `:198-246` (`validate_tool`)
- Test: `services/tool/src/service.rs` `mod tests` — the existing `FakeLlmRouter` gains a `decide` implementation

**Interfaces:**
- Consumes: `common::proto::llm_router::v1::{SystemOneServiceClient, DecideRequest, Question, Noul, answer::Answer as Given}`.
- Produces: `validate_tool` keeps `-> Result<(bool, String), ToolError>`.

- [ ] **Step 1: Write the failing tests**

The existing `serve_llm(reply)` mounts only `LlmRouterService`. Replace it with
`serve_llm_counting_decisions(noul: f64) -> (String, Arc<AtomicUsize>)`, which mounts **both**
services on one `ConnectRouter` — `Decide` answers `{"id": "meets_standard", "noul": {"noul": noul}}`
and `Complete` returns a fixed refusal sentence while bumping the counter. `draft_tool(&service)` is
the existing create-a-Draft-row helper (extract it from the current tests if it is inline), and
`OWNER` is a module-level `Uuid` the tests already use for the owning user. Then add: 

```rust
    #[tokio::test(flavor = "multi_thread")]
    async fn a_confident_yes_validates_without_asking_for_words() {
        let (llm_url, completions) = serve_llm_counting_decisions(0.93).await;
        let (_test, service) = service_with(&llm_url).await;
        let tool_id = draft_tool(&service).await;

        let (approved, feedback) = service.validate_tool(tool_id, OWNER).await.expect("validate");

        assert!(approved);
        assert!(feedback.is_empty(), "an approval needs no prose: {feedback:?}");
        assert_eq!(completions.load(Ordering::SeqCst), 0, "Complete was called on the happy path");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_refusal_spends_one_completion_on_the_reason() {
        let (llm_url, completions) = serve_llm_counting_decisions(0.08).await;
        let (_test, service) = service_with(&llm_url).await;
        let tool_id = draft_tool(&service).await;

        let (approved, feedback) = service.validate_tool(tool_id, OWNER).await.expect("validate");

        assert!(!approved);
        assert!(!feedback.is_empty());
        assert_eq!(completions.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_unreachable_decider_refuses_rather_than_approving() {
        let (_test, service) = service_with("http://127.0.0.1:1").await;
        let tool_id = draft_tool(&service).await;
        assert!(service.validate_tool(tool_id, OWNER).await.is_err());
    }
```

- [ ] **Step 2: Run them to verify they fail**

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo nextest run -p tool service::
```
Expected: FAIL — `validate_tool` still calls `Complete`, so the completion counter is 1 on the happy path.

- [ ] **Step 3: Implement**

Replace the `Complete` call in `validate_tool`:

```rust
const APPROVAL_THRESHOLD: f64 = 0.7;

// A tool definition either meets the standard or it does not: that is a calibrated yes/no, not
// prose. Words are only needed to explain a refusal, so they are only paid for on a refusal.
const VALIDATION_INSTRUCTIONS: &str = "Does this tool definition meet 2026 API design standards?";
const WHEN_TRUE: &str = "input_schema and output_schema are each a plausible JSON Schema object, the description clearly states what the tool does and, for write or destructive risk, what it changes, and timeout_seconds is plausible for that work";
const WHEN_FALSE: &str = "any of those is missing, vague or implausible";
```

```rust
        let mut request: DecideRequest = serde_json::from_value(serde_json::json!({
            "state": definition_state(&tool),
        }))
        .map_err(|e| ToolError::InvalidRequest(format!("could not build the decision: {e}")))?;
        let mut question: Question = serde_json::from_value(serde_json::json!({
            "instructions": VALIDATION_INSTRUCTIONS,
        }))
        .map_err(|e| ToolError::InvalidRequest(format!("could not build the question: {e}")))?;
        question.id = "meets_standard".to_owned();
        question.kind = Noul {
            when_true: Some(WHEN_TRUE.to_owned()),
            when_false: Some(WHEN_FALSE.to_owned()),
            ..Default::default()
        }
        .into();
        request.questions = vec![question];

        let decided = self
            .decider
            .decide(request)
            .await
            .map_err(|e| ToolError::InvalidRequest(format!("llm-router call failed: {e}")))?
            .into_owned();
        let approved = decided
            .answers
            .iter()
            .find(|answer| answer.id == "meets_standard")
            .and_then(|answer| match answer.answer.as_ref() {
                Some(Given::Noul(noul)) => Some(noul.noul),
                _ => None,
            })
            .map(|noul| noul >= APPROVAL_THRESHOLD)
            .ok_or_else(|| {
                ToolError::InvalidRequest("the decision carried no answer".to_owned())
            })?;
        let feedback = if approved {
            String::new()
        } else {
            self.refusal_words(&tool).await
        };
```

`definition_state` is the existing `user_prompt` string, renamed and returned as a `serde_json::Value::String`. `refusal_words` is the old `Complete` call with a system prompt narrowed to "explain in one or two sentences why this tool definition does not meet the standard", returning the content or a fixed fallback string when the call fails.

Add `decider: SystemOneServiceClient<HttpClient>` beside `llm` in `Service`, built in `Service::new` from the same `llm_router_url` with the same `ClientConfig`.

- [ ] **Step 4: Run the tests**

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo nextest run -p tool
cargo clippy -p tool --all-targets -- -D warnings
```
Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add services/tool/src/service.rs
git commit -m "tool: validation is a calibrated yes/no, and only a refusal costs words"
```

---

### Task 7: `intent.rs` — one `Decide` per turn

**Files:**
- Create: `services/chat/src/intent.rs`
- Delete: `services/chat/src/classifier.rs` (its `TopicSummary`, `Action`, `MAX_TITLE_CHARS`, `truncate`, `fallback`, `parse_actions` and multi-theme prompt move into `intent.rs`)
- Modify: `services/chat/src/lib.rs`
- Modify every importer of the old module — `rg -n 'classifier::' services/chat/src` lists exactly four:
  `main.rs:7` (`MAX_TITLE_CHARS`), `topic_watcher.rs:12` (`truncate`), `topic_turn.rs:14` (`Action, MAX_TITLE_CHARS, TopicSummary, truncate`), `topic_manager.rs:12` (`TopicClassifier` → `TopicIntent`, and the field rename)
- Modify: `services/chat/src/fakes.rs` — add `SystemOneService` to the **existing** `FakeLlmRouter`, not a separate type
- Test: inline tests in `services/chat/src/intent.rs`

**Interfaces:**
- Consumes: `common::proto::llm_router::v1::{SystemOneServiceClient, DecideRequest, Question, Noul, Choice, ChoiceOption, answer::Answer as Given}`.
- Produces: `chat::intent::{TopicIntent, Routing, Action, TopicSummary, MAX_TITLE_CHARS}` —
  `pub enum Routing { Clarify, Act(Vec<Action>) }`,
  `pub async fn TopicIntent::route(&self, topics: &[TopicSummary], focus: Option<i64>, message: &str) -> Routing`,
  `pub const CLARIFICATION_TEXT: &str`.
  `Action` keeps its existing shape (`Continue { topic_id: i64 }`, `New { title: String, question: String }`).

- [ ] **Step 1: Write the failing tests**

In `services/chat/src/intent.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    const CLARIFY_BELOW: f64 = ACTIONABLE_THRESHOLD - 0.1;

    fn topics() -> Vec<TopicSummary> {
        vec![TopicSummary {
            id: 7,
            title: "Weather in San Francisco today".to_owned(),
            status: crate::entity::topic::Status::Running,
            result_summary: None,
        }]
    }

    #[tokio::test]
    async fn nothing_to_act_on_and_nothing_to_continue_asks() {
        // No topics at all, so `route` is not even sent: a greeting in an empty session is the
        // clarification case.
        let (url, calls) = crate::fakes::serve_decider(vec![("actionable", CLARIFY_BELOW)]).await;
        let intent = TopicIntent::new(&url).expect("client");
        assert!(matches!(intent.route(&[], None, "hey").await, Routing::Clarify));
        assert_eq!(calls.completions(), 0, "a clarification must not spend a completion");
    }

    #[tokio::test]
    async fn a_fragment_aimed_at_a_live_topic_continues_it_instead_of_being_questioned() {
        // "and?" carries no request of its own, so `actionable` is low — but it plainly continues
        // the running topic, and interrogating the user about it is the failure this ordering
        // exists to prevent.
        let (url, _) = crate::fakes::serve_decider_choosing_with(
            "topic_7",
            0.9,
            vec![("actionable", CLARIFY_BELOW)],
        )
        .await;
        let intent = TopicIntent::new(&url).expect("client");
        let Routing::Act(actions) = intent.route(&topics(), Some(7), "and?").await else {
            panic!("a fragment aimed at a live topic must never clarify");
        };
        assert_eq!(actions, vec![Action::Continue { topic_id: 7 }]);
    }

    #[tokio::test]
    async fn a_confident_route_continues_that_topic() {
        let (url, _) = crate::fakes::serve_decider_choosing("topic_7", 0.9).await;
        let intent = TopicIntent::new(&url).expect("client");
        let Routing::Act(actions) = intent.route(&topics(), Some(7), "and?").await else {
            panic!("expected actions");
        };
        assert_eq!(actions, vec![Action::Continue { topic_id: 7 }]);
    }

    #[tokio::test]
    async fn a_new_topic_keeps_the_users_own_words_verbatim() {
        let (url, calls) = crate::fakes::serve_decider_choosing("new", 0.9).await;
        let intent = TopicIntent::new(&url).expect("client");
        let Routing::Act(actions) = intent
            .route(&topics(), Some(7), "What is Rust's latest stable version?")
            .await
        else {
            panic!("expected actions");
        };
        assert_eq!(
            actions,
            vec![Action::New {
                title: "What is Rust's latest stable version?".to_owned(),
                question: "What is Rust's latest stable version?".to_owned(),
            }]
        );
        assert_eq!(calls.completions(), 0, "one theme needs no completion");
    }

    #[tokio::test]
    async fn a_title_longer_than_the_limit_is_truncated_but_the_question_is_not() {
        let long = "x".repeat(MAX_TITLE_CHARS + 40);
        let (url, _) = crate::fakes::serve_decider_choosing("new", 0.9).await;
        let intent = TopicIntent::new(&url).expect("client");
        let Routing::Act(actions) = intent.route(&[], None, &long).await else {
            panic!("expected actions");
        };
        let Action::New { title, question } = &actions[0] else {
            panic!("expected a new topic");
        };
        assert_eq!(title.chars().count(), MAX_TITLE_CHARS);
        assert_eq!(question, &long);
    }

    #[tokio::test]
    async fn several_themes_are_the_one_case_that_completes() {
        let (url, calls) = crate::fakes::serve_decider_splitting(
            r#"{"actions":[{"kind":"new","title":"Claude Code","question":"What is Claude Code?"},
                           {"kind":"new","title":"Academy","question":"Which Academy courses exist?"}]}"#,
        )
        .await;
        let intent = TopicIntent::new(&url).expect("client");
        let Routing::Act(actions) = intent
            .route(&[], None, "What is Claude Code? And which Academy courses exist?")
            .await
        else {
            panic!("expected actions");
        };
        assert_eq!(actions.len(), 2);
        assert_eq!(calls.completions(), 1);
    }

    #[tokio::test]
    async fn an_unreachable_decider_continues_the_focus_instead_of_clarifying() {
        let intent = TopicIntent::new("http://127.0.0.1:1").expect("client");
        let Routing::Act(actions) = intent.route(&topics(), Some(7), "and?").await else {
            panic!("a failure must never clarify");
        };
        assert_eq!(actions, vec![Action::Continue { topic_id: 7 }]);
    }

    #[tokio::test]
    async fn a_topic_id_that_is_not_in_the_session_falls_back_to_the_focus() {
        let (url, _) = crate::fakes::serve_decider_choosing("topic_999", 0.9).await;
        let intent = TopicIntent::new(&url).expect("client");
        let Routing::Act(actions) = intent.route(&topics(), Some(7), "and?").await else {
            panic!("expected actions");
        };
        assert_eq!(actions, vec![Action::Continue { topic_id: 7 }]);
    }

    #[tokio::test]
    async fn an_unsure_route_continues_the_focus() {
        let (url, _) = crate::fakes::serve_decider_choosing("new", 0.2).await;
        let intent = TopicIntent::new(&url).expect("client");
        let Routing::Act(actions) = intent.route(&topics(), Some(7), "and?").await else {
            panic!("expected actions");
        };
        assert_eq!(actions, vec![Action::Continue { topic_id: 7 }]);
    }

    #[test]
    fn the_state_is_an_array_so_the_topic_order_survives() {
        let state = decision_state(&topics(), Some(7), "and?");
        assert!(state.is_array(), "an object's keys reach the model re-sorted: {state}");
    }
}
```

- [ ] **Step 2: Add the fake decider to `services/chat/src/fakes.rs`**

`FakeLlmRouter` implements `LlmRouterService` and is what `manager_with_router()` mounts. Add
`SystemOneService` **to that same type**, on the same `ConnectRouter`, with an `answer_decision(...)`
arm beside the existing `answer_with` and `hold_every_call_until`. A separate fake type would leave
every existing `topic_turn.rs` test talking to a router that cannot decide — see Task 8 Step 1.

Then add a call counter and three convenience constructors returning `(url, Calls)`:

```rust
/// Counts what a turn actually spent, so a test can assert that the fast path made one typed call
/// and nothing else.
#[derive(Clone, Default)]
pub struct Calls(Arc<AtomicUsize>);

impl Calls {
    #[must_use]
    pub fn completions(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }
}

/// Answers only the given nouls; no `route` answer, so nothing is chosen.
pub async fn serve_decider(nouls: Vec<(&'static str, f64)>) -> (String, Calls);

/// Answers `route` with `choice` and `confidence`, and `actionable` high.
pub async fn serve_decider_choosing(choice: &'static str, confidence: f64) -> (String, Calls);

/// As above, with the nouls scripted explicitly — used to prove that a low `actionable` does not
/// override a confident route.
pub async fn serve_decider_choosing_with(
    choice: &'static str,
    confidence: f64,
    nouls: Vec<(&'static str, f64)>,
) -> (String, Calls);

/// Answers `separate_themes` high, and returns `plan` from `Complete`.
pub async fn serve_decider_splitting(plan: &'static str) -> (String, Calls);
```

- [ ] **Step 3: Run the tests to verify they fail**

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo nextest run -p chat intent::
```
Expected: FAIL to compile — `TopicIntent`, `Routing`, `decision_state` are not defined.

- [ ] **Step 4: Write `intent.rs`**

Structure (the prompt constants and `fallback` move over from `classifier.rs` unchanged except that the system prompt is narrowed to splitting a message into themes):

```rust
// Routes one user turn with one typed decision. Three questions ride the same call because a second
// question is far cheaper than a second round trip, and the whole point here is that the user sees
// something back immediately.

const CALL_TIMEOUT: Duration = Duration::from_secs(30);
pub const MAX_TITLE_CHARS: usize = 60;

/// Asking a person to repeat themselves is cheap; telling them nothing while an agent burns a
/// minute on a greeting is not. So this threshold is high: below it we clarify, and being wrong
/// costs one extra exchange.
const ACTIONABLE_THRESHOLD: f64 = 0.35;
/// Below this the model cannot separate the options at all, and the deterministic focus rule is a
/// better answer than its guess.
const ROUTE_CONFIDENCE_THRESHOLD: f64 = 0.5;
/// Splitting a message that holds one theme costs a completion and produces two half-topics, so
/// this asks for real certainty.
const SEPARATE_THEMES_THRESHOLD: f64 = 0.75;

pub const CLARIFICATION_TEXT: &str =
    "I didn't catch a request in that — what would you like me to find out?";

const NEW_TOPIC_OPTION: &str = "new";
const TOPIC_OPTION_PREFIX: &str = "topic_";

pub enum Routing {
    /// Nothing to act on. No topic is created and no execution starts.
    Clarify,
    Act(Vec<Action>),
}
```

`route` does, in order:

1. Build the state with `decision_state`, which returns a **`serde_json::Value::Array`** — `[{"topics": [...]}, {"message": "..."}]`, topics in recency order with the focused one first, each `{"id", "title", "status", "focused"}` — because a `google.protobuf.Value` object arrives at the model with its keys alphabetised.
2. Build the `Question`s via the `serde_json::from_value` idiom in Global Constraints. `route`'s options are `repeated`: one `ChoiceOption` per topic named `topic_<id>` with the title as its description, then `new`. **When `topics` is empty, omit `route` entirely** — its only option would be `new`, whose `confidence` is degenerate and provider-dependent, and whose answer changes nothing.
3. `self.decider.decide(request).await` — on `Err`, log at `warn` and return `Routing::Act(fallback(topics, focus, message))`.
4. Read `route` first: `confidence` below `ROUTE_CONFIDENCE_THRESHOLD`, or a `topic_<id>` not in `topics`, → treat as unrouted. `topic_<id>` present and confident → `Routing::Act(vec![Action::Continue { topic_id }])`, **and this returns before `actionable` is ever consulted**.
5. `separate_themes` above `SEPARATE_THEMES_THRESHOLD` → `self.split(message).await`, which is the old `Complete` call with `parse_actions`; on any failure, fall through to step 6.
6. `actionable` below `ACTIONABLE_THRESHOLD` → `Routing::Clarify`. Reached only when nothing existing was chosen, so a fragment aimed at a live topic has already left at step 4.
7. Otherwise one `Action::New { title: truncate(message, MAX_TITLE_CHARS), question: message.to_owned() }`.
8. Each answer is read at its own step and only where that step needs it. A `route` answer missing at step 4 means "unrouted", not "fall back"; an `actionable` answer missing at step 6 means "do not clarify". A decision carrying **no** answers at all → `fallback`.

> **Why `route` is read before `actionable`, and it matters.** `classifier.rs`'s prompt spends most
> of its rules insisting that "i'm still waiting", "and?", "more details please" and "that's not
> what I asked" are ALWAYS `continue`. Every one of those is a fragment with no request in it, so a
> correctly calibrated `actionable` comes back **low** — and clarifying on it would interrogate the
> user about the most common turn shape in a live session. Putting `route` first means clarification
> can only happen when there was nothing to continue in the first place, which is the case it was
> designed for. This ordering is load-bearing; do not "simplify" it into a single early check.

- [ ] **Step 5: Run the tests**

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo nextest run -p chat
cargo clippy -p chat --all-targets -- -D warnings
```
Expected: the 9 new intent tests pass. `topic_turn.rs` will not compile yet — that is Task 8; if the crate does not build, move only the `classifier` → `intent` rename of the call site in `topic_turn.rs` forward into this step and leave the `Clarify` arm as `Routing::Clarify => Vec::new()` with a `// Task 8` marker.

- [ ] **Step 6: Commit**

```bash
git add services/chat/src/intent.rs services/chat/src/lib.rs services/chat/src/fakes.rs
git rm services/chat/src/classifier.rs
git commit -m "chat: one typed decision routes a turn, and asks when there is nothing to route"
```

---

### Task 8: The clarification path

**Files:**
- Modify: `common/proto/chat/v1/chat.proto:82-94`
- Modify: `services/chat/src/events.rs:10-23` (`TopicEventKind`)
- Modify: `services/chat/src/main.rs:97` (proto mapping)
- Modify: `services/chat/src/topic_turn.rs:27-65` (`send_turn`, `classify`)
- Modify: `services/frontend/` (render the event)
- Test: `services/chat/src/topic_turn.rs` `mod tests`

**Interfaces:**
- Consumes: `chat::intent::{Routing, CLARIFICATION_TEXT}` (Task 7).
- Produces: `TopicEventKind::ClarificationNeeded`; `send_turn` returns `Ok(vec![])` on that path.

- [ ] **Step 1: Write the failing test**

In `services/chat/src/topic_turn.rs`'s `mod tests`:

```rust
    #[tokio::test(flavor = "multi_thread")]
    async fn a_turn_with_nothing_to_act_on_starts_no_topic_and_says_so() {
        let (url, _) = crate::fakes::serve_decider(vec![("actionable", 0.1)]).await;
        let harness = harness_with_router(&url).await;
        let mut events = harness.manager.subscribe();

        let started = harness.manager.send_turn(OWNER, Uuid::new_v4(), "hey".to_owned()).await
            .expect("a clarification is a normal outcome, not an error");

        assert!(started.is_empty(), "no topic may be created: {started:?}");
        let event = events.recv().await.expect("an event");
        assert_eq!(event.kind, TopicEventKind::ClarificationNeeded);
        assert_eq!(event.topic_id, None, "a clarification belongs to the session, not a topic");
        assert_eq!(
            event.payload.get("text").and_then(serde_json::Value::as_str),
            Some(crate::intent::CLARIFICATION_TEXT)
        );
        let (_focus, topics) = harness.manager.session.get_session_view(OWNER).await.expect("view");
        assert!(topics.is_empty());
    }
```

- [ ] **Step 2: Run it to verify it fails**

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo nextest run -p chat topic_turn::
```
Expected: FAIL — `TopicEventKind::ClarificationNeeded` does not exist.

- [ ] **Step 3: Add the wire and internal event**

`common/proto/chat/v1/chat.proto`, inside `ChatEventKind`:

```proto
  // The turn carried no request to act on. Session-level: topic_id is empty.
  CHAT_EVENT_KIND_CLARIFICATION_NEEDED = 11;
```

`services/chat/src/events.rs`, in `TopicEventKind`, after `SessionReset`:

```rust
    ClarificationNeeded,
```

`services/chat/src/main.rs`, in the mapping:

```rust
        TopicEventKind::ClarificationNeeded => ChatEventKind::ClarificationNeeded,
```

- [ ] **Step 4: Branch `send_turn`**

Rename `TopicManager`'s `classifier: TopicClassifier` field to `intent: TopicIntent` (and its construction in `services/chat/src/main.rs` and `topic_manager.rs`), delete the private `classify` helper, and move its `TopicSummary` mapping into a free `fn summaries(topics: &[topic::Model]) -> Vec<TopicSummary>`. Then replace the `classify` call in `send_turn`:

```rust
        let actions = match self.intent.route(&summaries(&topics), focus_topic_id, &content).await {
            // A turn with nothing to act on is answered here and now: creating a topic to ask a
            // question would cost an execution and a minute for a reply the session already has.
            // It records no message row, so a client retrying the same turn_id asks again — the
            // answer is identical and starts nothing, which is cheaper than indexing event payloads
            // by turn.
            Routing::Clarify => {
                self.publish(
                    TopicEvent::session_wide(session_id, TopicEventKind::ClarificationNeeded)
                        .with_payload(serde_json::json!({
                            "text": crate::intent::CLARIFICATION_TEXT,
                            "turn_id": turn_id,
                        })),
                )
                .await;
                return Ok(Vec::new());
            }
            Routing::Act(actions) => actions,
        };
```

> `TopicManager::publish` (`services/chat/src/topic_events.rs:15`) is `pub(crate) async fn
> publish(&self, event: TopicEvent)` — **infallible and on the manager**, so there is no `?` and it
> is not reached through `self.session`. It persists and then broadcasts; `SessionReset`
> (`topic_manager.rs:194`) deliberately bypasses persistence via `self.events.publish`, which a
> clarification must **not** do — the transcript has to survive a reconnect. The id it takes is the
> **session** uuid, not `user_id`.

Add `TopicEvent::session_wide(session_id, kind)` beside the existing constructors in `events.rs`, setting `topic_id: None`.

The Clarify branch runs **outside** `lock_turn`, so two concurrent retries of one `turn_id` publish two clarification events. That is acceptable and intentional — the event carries no side effect and starts nothing — and the comment above says so.

- [ ] **Step 5: Render it in the frontend**

In `services/frontend/`, find where `topic_completed` is turned into a transcript line and add a branch for `CHAT_EVENT_KIND_CLARIFICATION_NEEDED` that renders `payload.text` as an assistant reply with no topic heading.

- [ ] **Step 6: Re-script the two existing tests that a Decide-first turn breaks**

Every turn now calls `Decide` before `Complete`, so two tests in `services/chat/src/topic_turn.rs` no longer test what they were written to test. **They must be edited, not "expected to pass":**

- `a_message_naming_two_themes_opens_two_topics_and_focuses_the_first` (≈line 643) scripts a two-action plan through `router.answer_with`. Add `router.answer_decision(...)` arming `separate_themes` above `SEPARATE_THEMES_THRESHOLD`, or the turn never reaches `Complete` and opens one topic instead of two.
- `both_retries_of_one_turn_reach_the_classifier_before_either_takes_the_turn_lock` (≈line 351) parks both retries on a `Barrier` inside `Complete` via `hold_every_call_until`. `Complete` is no longer on the normal path, so the barrier is never reached and the test **hangs** to its `CLASSIFY_DEADLINE`. Move the hold onto `Decide` and rename the test to `…reach_the_decider_before…`.

Three more (`a_first_message…`, `the_session_view…`, `a_continue_on_the_running_focus…`) would pass on fallback alone, which means they stop testing routing. Arm `answer_decision` in each so they exercise the decision they claim to.

- [ ] **Step 7: Run the tests**

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo nextest run -p chat
cargo nextest run -p gateway
cargo clippy --workspace --all-targets --all-features -- -D warnings
```
Expected: all pass. The advisory-fallback tests (unreachable router, unusable output, unknown `topic_id`) keep their **assertions** unchanged — only their scripting changes.

- [ ] **Step 8: Commit**

```bash
git add common/proto/chat/v1/chat.proto services/chat/src services/frontend
git commit -m "chat: a turn with no request is answered in the transcript, not by an agent"
```

---

### Task 9: Documentation

**Files:**
- Modify: `services/tool/README.md`
- Modify: `services/chat/README.md`
- Modify: `services/llm-router/README.md`
- Modify: `docs/architecture.md`

- [ ] **Step 1: `services/tool/README.md`**

The "Capabilities, Not Topics" section currently says a `weather` or `stock_quote` tool would be `web_fetch` with a topic glued on. Rewrite that section to say what is now true: the **code** holds capabilities only — `race`, one declarative executor — and a subject reaches the system as a registry row, the same way a user-created tool does. Say explicitly that `CreateTool` does not accept `sources`, and why (arbitrary outbound HTTP on user input is its own SSRF and quota decision).

Add a `race` section: factories not futures, `fan_out` as width with reserve, `take` as quorum, losers cancelled rather than awaited, `Outcome` rather than `Result` because a partial answer is normal. And a `Declarative Tools` section documenting the `sources` shape, `{field}` substitution against `input_schema`, `pick`, and why two answers are returned as two.

- [ ] **Step 2: `services/chat/README.md`**

Replace the classifier section: one `Decide` per turn with three questions, the thresholds and what each one costs to get wrong, the one branch that reaches `Complete`, and — kept prominent — that classification is still advisory and a decider failure continues the focus. Add that a clarification is the single load-bearing outcome and records no message row, so a repeated `turn_id` re-asks.

- [ ] **Step 3: `services/llm-router/README.md`**

The System One section says the class exists; add that Chat and Tool are its callers, so a reader knows the class is live rather than aspirational.

- [ ] **Step 4: `docs/architecture.md`**

The Runtime Flow diagram predates Chat, Engine and Tool. Add them and their edges: Gateway → Chat, Chat → Engine, Engine → Tool and → LLM Router, Tool → Knowledge Base and → LLM Router. Note in Boundaries that Chat and Tool both call `SystemOneService` for decisions and `LlmRouterService` only where words are wanted.

- [ ] **Step 5: Run the full suite**

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo nextest run --workspace
cargo deny check
cargo machete
```
Expected: all green.

- [ ] **Step 6: Commit**

```bash
git add services/tool/README.md services/chat/README.md services/llm-router/README.md docs/architecture.md
git commit -m "docs: declarative tools, the race primitive and the typed intent path"
```

---

## Review

After Task 9, run the repository's review gates before pushing — entry point and required evidence format in `.agents/skills/code-review/SKILL.md`. Pay particular attention to `gate-architecture`'s "Capabilities, Not Topics": the only place any subject may appear in this diff is a seed literal's `name`, `description` and `url` in `services/tool/src/main.rs`.
