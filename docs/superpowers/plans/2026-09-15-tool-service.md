# Tool Service Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build `services/tool` — a tool registry + executor with deterministic risk policy —
and wire it into the already-running `engine` service as a real `kind = "tool"` `TaskExecutor`,
so `agent_graph()`/`rag_graph()` work end-to-end.

**Architecture:** A stateless Connect RPC service (`CreateTool`/`ValidateTool`/`ActivateTool`/
`ListTools`/`Execute`) backed by one reference table (`tool.tools`). System tools
(`web_search`/`web_fetch`/`kb_search`/`kb_read_document`) are seeded at startup, same pattern as
Engine's built-in graphs, and dispatched to real Rust implementations by slug. `services/engine`
gains a second `TaskExecutor` (`ToolTaskExecutor`) and its `Dispatcher` injects a tool catalog
into the LLM's prompt before every tool-calling turn.

**Tech Stack:** Rust 2024, SeaORM (entity-first schema-sync), Connect RPC (`connectrpc` +
`buffa`), Postgres, testcontainers. No new external crate beyond `reqwest` (already a workspace
dependency).

**Spec:** `docs/superpowers/specs/2026-09-15-tool-service-design.md` — read it before starting.
Do not read or use `docs/superpowers/specs/2026-09-16-tool-service-design.md` — a different,
independent draft from a separate session; not the spec this plan implements.

## Global Constraints

- Strict lints: `unsafe_code = "forbid"`, clippy `all = deny`, `unwrap_used = "deny"`,
  `too_many_lines = "deny"`, `cognitive_complexity = "deny"` — every new file.
- No `async_trait`: ports use `impl Future<Output = ...> + Send` in return position.
- Entity-first SeaORM: `db.get_schema_registry("tool::entity::*").sync(&db)`, no hand-written
  `.sql` migrations.
- Every JSON column is `jsonb` (`column_type = "JsonBinary"`) — no exceptions
  (`.agents/skills/code-review/gate-database/SKILL.md`).
- `tools` uniqueness per owner cannot use SeaORM's `unique_key` (Postgres treats `NULL <> NULL`,
  so two system tools could share a slug) — a manual `CREATE UNIQUE INDEX` on
  `COALESCE(user_id, '00000000-0000-0000-0000-000000000000'::uuid), slug` after schema-sync,
  the same sanctioned exception already used for Engine's partial index and partitions.
- Work directly on `main`, no worktree/branch — Engine's worktree is already removed after its
  squash; do the same here (commit directly, no branch to merge later).
- Docker: `docker compose -p aiengineeringboilerplate` — the real running stack
  (postgres/crawler/gateway/knowledge-base/llm-router/engine, 6 services) — never
  `docker compose down -v`, never touch data outside the `tool` schema.
- Commits: exact message given per task, plus trailers after a blank line:
  `Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>` /
  `Claude-Session: https://claude.ai/code/session_014WYAbXrDaF8pTeCytR8Uqg`. Stage only the
  files a task lists; never `git add -A`.
- `export PATH="$HOME/.cargo/bin:$PATH"` before any `cargo` command in every task.

---

## Task 1: Promote `Principal` to `common::principal`

**Files:**
- Create: `common/src/principal.rs`
- Modify: `common/src/lib.rs` (add `pub mod principal;`)
- Modify: `services/engine/src/principal.rs` → delete; replace every `use crate::principal::` /
  `principal::` reference in `services/engine/src/*.rs` with `common::principal::`
- Modify: `services/engine/src/lib.rs` (remove `pub mod principal;`)

`services/tool` needs the exact same "read `user_id`/`session_id` from Connect metadata, never
trust the request body" logic `services/engine/src/principal.rs` already has — the second real
consumer the Engine spec's own promotion rule was written for. Move the file verbatim (same
`Principal { user_id: Uuid, session_id: String }`, `USER_ID_HEADER`/`SESSION_ID_HEADER` consts,
`from_metadata`, same three tests) into `common`, no behavior change.

- [ ] **Step 1: Move the file**

`git mv services/engine/src/principal.rs common/src/principal.rs` — keep every line identical
except the module doc comment, which gains one sentence:
```rust
// Reads the two headers Gateway will stamp on every proxied request once it starts fronting
// Engine/Tool Service — trusted, not the request body. Shared by every service that reads
// Principal from Connect metadata (engine, tool — see each spec's "Principal и user_id").
```
(replacing the file's original single-line header comment).

- [ ] **Step 2: Wire the module**

Add `pub mod principal;` to `common/src/lib.rs` (alphabetical with `cache`/`llm`/`logging`).
Remove `pub mod principal;` from `services/engine/src/lib.rs`.

- [ ] **Step 3: Fix every call site in `services/engine`**

Run `grep -rn "principal::" services/engine/src` — every hit changes `crate::principal` (or a
bare `principal::` import already in scope via `use crate::principal;`) to `common::principal`.
Based on the existing code, this touches `services/engine/src/main.rs` (`principal::from_metadata`
calls in `start_execution`/`stream_events`/`create_schedule`/`list_schedules`) and any
`use crate::principal::Principal;` line. `common` is already a dependency of `services/engine/Cargo.toml`
(`common = { workspace = true }`) — no Cargo.toml change needed.

- [ ] **Step 4: Verify**

Run: `cargo test -p common -p engine --all-features 2>&1 | grep -E "^test result|^error"`
Expected: `common`'s 3 principal tests now appear under `common`; `engine`'s full suite (378
tests) still passes unchanged — this task moves code, it does not change behavior.

- [ ] **Step 5: Commit**

```bash
git add common/src/principal.rs common/src/lib.rs services/engine/src/lib.rs services/engine/src/main.rs
git commit -m "common: promote Principal from services/engine — second real consumer is Tool Service"
```

## Task 2: Move `extract()` to `common::extract`

**Files:**
- Create: `common/src/extract.rs`
- Modify: `common/src/lib.rs` (add `pub mod extract;`)
- Delete: `services/crawler/src/extract.rs`
- Modify: every `services/crawler/src/*.rs` file importing `crate::extract` → `common::extract`

Same promotion rule, second consumer this time is `web_fetch`. Move the file exactly as it is
today (`pub struct Extracted { title, main_text, content_hash }`, `pub fn extract(url: &str, html:
&str) -> Extracted`, its existing tests) — no behavior change, `crawler` must produce identical
output before and after.

- [ ] **Step 1: Move**

`git mv services/crawler/src/extract.rs common/src/extract.rs`. Update its header comment:
```rust
// Turns fetched HTML into a title, the main text, and a hash of that text. Shared by the
// crawler (a fetched page) and Tool Service's web_fetch (a tool call's fetched page) — the
// second real consumer this moved for.
```

- [ ] **Step 2: Wire and fix call sites**

Add `pub mod extract;` to `common/src/lib.rs`. Run `grep -rln "crate::extract\|mod extract" services/crawler/src`
and update every hit to `common::extract`; remove `mod extract;` from `services/crawler/src/main.rs`
(or wherever it's declared).

- [ ] **Step 3: Verify**

Run: `cargo test -p common -p crawler --all-features 2>&1 | grep -E "^test result|^error"`
Expected: `extract`'s tests now run under `common`; `crawler`'s full suite (102 tests) unchanged.

- [ ] **Step 4: Commit**

```bash
git add common/src/extract.rs common/src/lib.rs services/crawler/src
git commit -m "common: promote extract() from crawler — second real consumer is Tool Service's web_fetch"
```

## Task 3: `tool` crate scaffold and the `tools` entity

**Files:**
- Create: `services/tool/Cargo.toml`
- Create: `services/tool/src/lib.rs`
- Create: `services/tool/src/main.rs`
- Create: `services/tool/src/entity/mod.rs`
- Create: `services/tool/src/entity/tool.rs`
- Modify: root `Cargo.toml` (add `"services/tool"` to `[workspace] members`)

Mirrors `services/engine`'s Task 10 shape exactly: lib+bin split from the start (later tasks each
add their own `pub mod` line to `lib.rs`), entity-first, schema-sync + one manual index statement
in `main.rs`.

- [ ] **Step 1: Workspace wiring and `Cargo.toml`**

Add `"services/tool"` to root `Cargo.toml`'s `[workspace] members`.

`services/tool/Cargo.toml`:
```toml
[package]
name = "tool"
edition.workspace = true
rust-version.workspace = true
version.workspace = true
publish.workspace = true

[lib]
name = "tool"
path = "src/lib.rs"

[lints]
workspace = true

[dependencies]
axum = { workspace = true }
buffa = { workspace = true }
chrono = { workspace = true }
common = { workspace = true }
connectrpc = { workspace = true, features = ["axum", "client"] }
reqwest = { workspace = true }
sea-orm = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
thiserror = { workspace = true }
tokio = { workspace = true, features = ["macros", "rt-multi-thread"] }
tracing = { workspace = true }
uuid = { workspace = true }

[features]
test-support = ["common/test-support"]

[dev-dependencies]
common = { workspace = true, features = ["test-support"] }
tokio = { workspace = true, features = ["test-util"] }
```

- [ ] **Step 2: The entity**

`services/tool/src/entity/tool.rs`:
```rust
// tool.tools: the registry — one row per tool, system (user_id NULL) or user-owned. Reference
// data, grows with tool count, not with time — no partitioning needed (gate-database "Growth").
// Uniqueness per owner is NOT expressed here via unique_key (Postgres treats NULL <> NULL, so
// two system tools could share a slug) — see the manual index in main.rs.

use sea_orm::entity::prelude::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "i16", db_type = "SmallInteger")]
pub enum Risk {
    #[sea_orm(num_value = 0)]
    ReadOnly,
    #[sea_orm(num_value = 1)]
    Write,
    #[sea_orm(num_value = 2)]
    Destructive,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "i16", db_type = "SmallInteger")]
pub enum Status {
    #[sea_orm(num_value = 0)]
    Draft,
    #[sea_orm(num_value = 1)]
    Validated,
    #[sea_orm(num_value = 2)]
    Active,
}

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "tools", schema_name = "tool")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub user_id: Option<Uuid>,
    #[sea_orm(column_type = "String(StringLen::N(64))")]
    pub slug: String,
    #[sea_orm(column_type = "String(StringLen::N(128))")]
    pub name: String,
    #[sea_orm(column_type = "Text")]
    pub description: String,
    #[sea_orm(column_type = "JsonBinary")]
    pub input_schema: Json,
    #[sea_orm(column_type = "JsonBinary")]
    pub output_schema: Json,
    pub connection_id: Option<i64>,
    pub risk: Risk,
    pub timeout_seconds: i32,
    pub status: Status,
    pub created_at: DateTimeUtc,
    pub updated_at: DateTimeUtc,
}

impl ActiveModelBehavior for ActiveModel {}

/// The uniqueness `unique_key` cannot express — see this file's header comment. Run once, right
/// after schema-sync, same sanctioned mechanism as `services/engine`'s partial index.
pub const INDEX_STATEMENTS_CREATED_AFTER_SCHEMA_SYNC: [&str; 1] = [
    "CREATE UNIQUE INDEX IF NOT EXISTS tools_owner_slug_idx ON tool.tools \
     (COALESCE(user_id, '00000000-0000-0000-0000-000000000000'::uuid), slug)",
];
```

`services/tool/src/entity/mod.rs`:
```rust
pub mod tool;
```

- [ ] **Step 3: `lib.rs` and a schema-sync-only `main.rs`**

`services/tool/src/lib.rs`:
```rust
// tool as a library: everything main.rs assembles, exposed for unit- and integration-testing
// directly (main.rs stays a thin binary entry point). Each later task adds its own `pub mod`.

pub mod entity;
```

`services/tool/src/main.rs`:
```rust
// tool: the tool registry + executor service. Connects to Postgres, syncs its schema, applies
// the one manual uniqueness index schema-sync cannot express. This early version only proves
// the crate and its entity compile and the schema comes up.

use sea_orm::{ConnectionTrait, Database};

use tool::entity::tool::INDEX_STATEMENTS_CREATED_AFTER_SCHEMA_SYNC;

fn env(name: &str) -> Result<String, String> {
    std::env::var(name).map_err(|_| format!("{name} is not set"))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    common::logging::init();

    let database_url = env("DATABASE_URL")?;
    let db = Database::connect(&database_url).await?;
    db.get_schema_registry("tool::entity::*").sync(&db).await?;
    for statement in INDEX_STATEMENTS_CREATED_AFTER_SCHEMA_SYNC {
        db.execute_unprepared(statement).await?;
    }

    tracing::info!("tool: schema synced, no RPC surface yet");
    Ok(())
}
```

- [ ] **Step 4: Verify**

Run: `cargo check -p tool 2>&1 | tail -20`
Expected: compiles clean.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock services/tool/
git commit -m "tool: crate scaffold, tools entity, schema-sync main"
```

## Task 4: `common/proto/tools/v1/tools.proto`

**Files:**
- Create: `common/proto/tools/v1/tools.proto`
- Modify: `common/build.rs`

**Interfaces:**
- Produces: `common::proto::tools::v1::{ToolService, ToolServiceClient, CreateToolRequest,
  CreateToolResponse, ValidateToolRequest, ValidateToolResponse, ActivateToolRequest,
  ActivateToolResponse, ListToolsRequest, ListToolsResponse, Tool, ExecuteRequest,
  ExecuteResponse}` — generated, consumed from Task 9 onward.

- [ ] **Step 1: Write the proto file**

`common/proto/tools/v1/tools.proto`:
```proto
syntax = "proto3";

package tools.v1;

service ToolService {
  rpc CreateTool(CreateToolRequest) returns (CreateToolResponse);
  rpc ValidateTool(ValidateToolRequest) returns (ValidateToolResponse);
  rpc ActivateTool(ActivateToolRequest) returns (ActivateToolResponse);
  rpc ListTools(ListToolsRequest) returns (ListToolsResponse);
  rpc Execute(ExecuteRequest) returns (ExecuteResponse);
}

message CreateToolRequest {
  string slug = 1;
  string name = 2;
  string description = 3;
  string input_schema_json = 4;
  string output_schema_json = 5;
  string risk = 6;             // "read_only" | "write" | "destructive"
  int32 timeout_seconds = 7;
}
message CreateToolResponse {
  string tool_id = 1;
}

message ValidateToolRequest {
  string tool_id = 1;
}
message ValidateToolResponse {
  bool approved = 1;
  string feedback = 2;
}

message ActivateToolRequest {
  string tool_id = 1;
}
message ActivateToolResponse {}

message ListToolsRequest {}
message ListToolsResponse {
  repeated Tool tools = 1;
}
message Tool {
  string id = 1;
  string slug = 2;
  string name = 3;
  string description = 4;
  string input_schema_json = 5;
  string risk = 6;
  string status = 7;           // "draft" | "validated" | "active"
}

message ExecuteRequest {
  string slug = 1;
  string input_json = 2;
  string idempotency_key = 3;
}
message ExecuteResponse {
  string status = 1;           // "ok" | "requires_approval" | "error" | "not_executable"
  string output_json = 2;
  string error_message = 3;
}
```

- [ ] **Step 2: Wire codegen**

`common/build.rs` — add `"proto/tools/v1/tools.proto"` to the `.files(&[...])` list alongside
the existing four proto files.

- [ ] **Step 3: Verify**

Run: `cargo check -p common 2>&1 | tail -20`
Expected: compiles clean, `common::proto::tools::v1::*` now exists.

- [ ] **Step 4: Commit**

```bash
git add common/proto/tools/v1/tools.proto common/build.rs
git commit -m "common: add tools/v1 proto contract"
```

## Task 5: `policy.rs` — deterministic risk policy

**Files:**
- Create: `services/tool/src/policy.rs`

**Interfaces:**
- Consumes: `entity::tool::Risk` (Task 3).
- Produces: `enum PolicyDecision { Execute, RequiresApproval }`,
  `fn check_policy(risk: Risk) -> PolicyDecision`.

- [ ] **Step 1: Write the failing tests**

`services/tool/src/policy.rs`:
```rust
// Risk policy: deterministic Rust, never an LLM call. ReadOnly runs; Write/Destructive need an
// approval flow Engine doesn't have yet (see the spec's "Три развилки", #2) — Execute reports
// that plainly rather than running the tool or hanging.

use crate::entity::tool::Risk;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyDecision {
    Execute,
    RequiresApproval,
}

#[must_use]
pub fn check_policy(risk: Risk) -> PolicyDecision {
    match risk {
        Risk::ReadOnly => PolicyDecision::Execute,
        Risk::Write | Risk::Destructive => PolicyDecision::RequiresApproval,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_executes() {
        assert_eq!(check_policy(Risk::ReadOnly), PolicyDecision::Execute);
    }

    #[test]
    fn write_requires_approval() {
        assert_eq!(check_policy(Risk::Write), PolicyDecision::RequiresApproval);
    }

    #[test]
    fn destructive_requires_approval() {
        assert_eq!(check_policy(Risk::Destructive), PolicyDecision::RequiresApproval);
    }
}
```

Add `pub mod policy;` to `services/tool/src/lib.rs`.

- [ ] **Step 2: Run**

Run: `cargo test -p tool policy:: 2>&1 | tail -15`
Expected: PASS, 3 tests.

- [ ] **Step 3: Commit**

```bash
git add services/tool/src/policy.rs services/tool/src/lib.rs
git commit -m "tool: deterministic risk policy"
```

## Task 6: `web_search` — Brave adapter behind a `SearchProvider` trait

**Files:**
- Create: `services/tool/src/providers/mod.rs`
- Create: `services/tool/src/providers/brave.rs`

**Interfaces:**
- Produces: `trait SearchProvider { fn search(&self, query: &str, limit: u8) -> impl Future<Output
  = Result<Vec<SearchResult>, String>> + Send; }`, `struct SearchResult { title: String, url:
  String, snippet: String }`, `struct BraveSearchProvider { api_key: String, base_url: String,
  client: reqwest::Client }` implementing it.

Confirmed against Brave's own docs (not guessed): `GET https://api.search.brave.com/res/v1/web/search`,
header `X-Subscription-Token: <key>`, query params `q` (query) and `count` (results, max 20),
response `{"web": {"results": [{"title", "url", "description"}]}}`.

- [ ] **Step 1: Write the failing test — fake HTTP server, no real key needed**

`services/tool/src/providers/brave.rs`:
```rust
// web_search: Brave Search API behind SearchProvider, so a future provider swap doesn't touch
// the tool that calls it. BRAVE_SEARCH_API_KEY lives directly in this service's env today —
// documented as temporary, moves to Integrations Service once that exists (spec, "Что это").

use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

pub trait SearchProvider: Send + Sync {
    fn search(&self, query: &str, limit: u8) -> impl Future<Output = Result<Vec<SearchResult>, String>> + Send;
}

pub struct BraveSearchProvider {
    api_key: String,
    base_url: String,
    client: reqwest::Client,
}

impl BraveSearchProvider {
    #[must_use]
    pub fn new(api_key: String) -> Self {
        Self { api_key, base_url: "https://api.search.brave.com/res/v1/web/search".to_owned(), client: reqwest::Client::new() }
    }

    #[must_use]
    fn with_base_url(api_key: String, base_url: String) -> Self {
        Self { api_key, base_url, client: reqwest::Client::new() }
    }
}

#[derive(Deserialize)]
struct BraveResponse {
    web: Option<BraveWeb>,
}
#[derive(Deserialize)]
struct BraveWeb {
    results: Vec<BraveResult>,
}
#[derive(Deserialize)]
struct BraveResult {
    title: String,
    url: String,
    description: String,
}

impl SearchProvider for BraveSearchProvider {
    async fn search(&self, query: &str, limit: u8) -> Result<Vec<SearchResult>, String> {
        let response = self
            .client
            .get(&self.base_url)
            .header("X-Subscription-Token", &self.api_key)
            .header("Accept", "application/json")
            .query(&[("q", query), ("count", &limit.to_string())])
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !response.status().is_success() {
            return Err(format!("brave search returned {}", response.status()));
        }
        let parsed: BraveResponse = response.json().await.map_err(|e| e.to_string())?;
        Ok(parsed
            .web
            .map(|web| web.results)
            .unwrap_or_default()
            .into_iter()
            .map(|r| SearchResult { title: r.title, url: r.url, snippet: r.description })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::Query;
    use axum::response::Json;
    use std::collections::HashMap;

    async fn fake_brave(Query(params): Query<HashMap<String, String>>) -> Json<serde_json::Value> {
        assert_eq!(params.get("q").map(String::as_str), Some("rust async traits"));
        Json(serde_json::json!({
            "web": {"results": [{"title": "Async traits", "url": "https://example.com/a", "description": "A guide."}]}
        }))
    }

    async fn serve() -> String {
        let app = axum::Router::new().route("/res/v1/web/search", axum::routing::get(fake_brave));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("addr");
        tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
        format!("http://{address}/res/v1/web/search")
    }

    #[tokio::test]
    async fn search_parses_brave_results() {
        let url = serve().await;
        let provider = BraveSearchProvider::with_base_url("fake-key".to_owned(), url);

        let results = provider.search("rust async traits", 5).await.expect("search");

        assert_eq!(results, vec![SearchResult {
            title: "Async traits".to_owned(),
            url: "https://example.com/a".to_owned(),
            snippet: "A guide.".to_owned(),
        }]);
    }
}
```

`services/tool/src/providers/mod.rs`:
```rust
pub mod brave;
```

Add `pub mod providers;` to `services/tool/src/lib.rs`. Add `axum = { workspace = true }` to
`services/tool/Cargo.toml`'s `[dev-dependencies]` (only test code spins the fake server).

- [ ] **Step 2: Run**

Run: `cargo test -p tool providers::brave:: 2>&1 | tail -20`
Expected: PASS, 1 test. No network call beyond `127.0.0.1`, no real Brave key needed.

- [ ] **Step 3: Commit**

```bash
git add services/tool/src/providers services/tool/src/lib.rs services/tool/Cargo.toml
git commit -m "tool: web_search — Brave adapter behind SearchProvider"
```

## Task 7: `web_fetch` — reqwest + `common::extract`

**Files:**
- Create: `services/tool/src/tools/mod.rs`
- Create: `services/tool/src/tools/web_fetch.rs`

**Interfaces:**
- Consumes: `common::extract::extract` (Task 2).
- Produces: `async fn fetch(client: &reqwest::Client, url: &str) -> Result<FetchedPage, String>`,
  `struct FetchedPage { title: String, text: String }`.

- [ ] **Step 1: Write the failing test**

`services/tool/src/tools/web_fetch.rs`:
```rust
// web_fetch: GET a URL, extract its readable text the same way the crawler does (common::extract
// — the second consumer that moved it there). A byte cap keeps a pathological page from being
// pulled fully into memory before extraction, same order of magnitude as the crawler's own cap.

const MAX_FETCH_BYTES: usize = 5 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct FetchedPage {
    pub title: String,
    pub text: String,
}

pub async fn fetch(client: &reqwest::Client, url: &str) -> Result<FetchedPage, String> {
    let response = client.get(url).send().await.map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        return Err(format!("fetch {url} returned {}", response.status()));
    }
    let bytes = response.bytes().await.map_err(|e| e.to_string())?;
    if bytes.len() > MAX_FETCH_BYTES {
        return Err(format!("{url} exceeded {MAX_FETCH_BYTES} bytes, refusing to parse"));
    }
    let html = String::from_utf8_lossy(&bytes);
    let extracted = common::extract::extract(url, &html);
    Ok(FetchedPage { title: extracted.title, text: extracted.main_text })
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn serve(html: &'static str) -> String {
        let app = axum::Router::new().route(
            "/page",
            axum::routing::get(|| async { axum::response::Html(html) }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("addr");
        tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
        format!("http://{address}/page")
    }

    #[tokio::test]
    async fn fetch_extracts_title_and_text() {
        let url = serve(
            "<html><head><title>Hi</title></head><body><article><h1>Hi</h1><p>Borrowing lets code use a value without taking ownership of it. References must always be valid, which the borrow checker enforces at compile time.</p></article></body></html>",
        )
        .await;
        let client = reqwest::Client::new();

        let page = fetch(&client, &url).await.expect("fetch");

        assert!(page.text.contains("Borrowing lets code use a value"));
    }

    #[tokio::test]
    async fn a_non_success_status_is_an_error() {
        let app = axum::Router::new().route("/missing", axum::routing::get(|| async { axum::http::StatusCode::NOT_FOUND }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("addr");
        tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
        let client = reqwest::Client::new();

        let error = fetch(&client, &format!("http://{address}/missing")).await.unwrap_err();

        assert!(error.contains("404"));
    }
}
```

`services/tool/src/tools/mod.rs`:
```rust
pub mod web_fetch;
```

Add `pub mod tools;` to `services/tool/src/lib.rs`.

- [ ] **Step 2: Run**

Run: `cargo test -p tool tools::web_fetch:: 2>&1 | tail -20`
Expected: PASS, 2 tests.

- [ ] **Step 3: Commit**

```bash
git add services/tool/src/tools
git commit -m "tool: web_fetch — reqwest + common::extract"
```

## Task 8: `kb_search` / `kb_read_document` — knowledge-base client

**Files:**
- Create: `services/tool/src/tools/kb_client.rs`

**Interfaces:**
- Produces: `struct KnowledgeBaseClient { inner: KnowledgeBaseServiceClient<HttpClient> }` with
  `fn new(url: &str) -> Result<Self, String>`, `async fn search(&self, query: &str, limit: u8) ->
  Result<Vec<SearchResultRow>, String>`, `async fn read_document(&self, source: &str, source_id:
  &str, version: &str) -> Result<String, String>`. `struct SearchResultRow { title: String,
  snippet: String, source: String, source_id: String }` — matches
  `common/proto/knowledge_base/v1/knowledge_base.proto`'s real `SearchResult`/`DocumentRef`
  shape (already read below): knowledge-base results have no `url` field, only `document {
  source, source_id, version }`, so this row does NOT mirror `web_search`'s `SearchResult`
  field-for-field — that assumption from the spec turned out wrong once the real proto was read;
  the two tools' JSON output legitimately differ in shape, which is fine, the LLM sees both
  shapes through their own `output_schema`.

The real proto (`common/proto/knowledge_base/v1/knowledge_base.proto`, already confirmed, not to
be re-derived): `SearchRequest { query, page_types (repeated string), limit (uint32, 0 = 10, max
50) }`; `SearchResponse { results: repeated SearchResult }`; `SearchResult { source, source_id,
title, summary, page_type, score, keywords, snippet, updated_at, document: DocumentRef, 
passage_ordinal }`; `DocumentRef { source, source_id, version }`; `ReadDocumentRequest {
document: DocumentRef, cursor, max_chars (uint32, 0 = 16000, max 50000) }`;
`ReadDocumentResponse { document, title, content, next_cursor, total_chars }`. This task reads
only the first page of a document (`cursor = ""`, default `max_chars`) — pagination is not
needed for a tool call feeding an LLM's context window.

- [ ] **Step 1: Write the failing test — fake `KnowledgeBaseService`, mirroring `llm_router_client.rs`'s `FakeLlmRouter` pattern**

`services/tool/src/tools/kb_client.rs`:
```rust
// kb_search / kb_read_document: thin wrappers over the knowledge_base.v1.KnowledgeBaseService
// this stack already runs — same client-construction pattern as
// services/knowledge-base/src/llm_router_client.rs uses for llm-router.

use common::proto::knowledge_base::v1::{
    DocumentRef, KnowledgeBaseServiceClient, ReadDocumentRequest, SearchRequest,
};
use connectrpc::Protocol;
use connectrpc::client::{ClientConfig, HttpClient};
use std::time::Duration;

const CALL_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct SearchResultRow {
    pub title: String,
    pub snippet: String,
    pub source: String,
    pub source_id: String,
}

pub struct KnowledgeBaseClient {
    inner: KnowledgeBaseServiceClient<HttpClient>,
}

impl KnowledgeBaseClient {
    pub fn new(url: &str) -> Result<Self, String> {
        let target = url.parse().map_err(|e| format!("could not parse knowledge-base URL {url:?}: {e}"))?;
        Ok(Self {
            inner: KnowledgeBaseServiceClient::new(
                HttpClient::plaintext_http2_only(),
                ClientConfig::new(target).with_protocol(Protocol::Grpc).with_default_timeout(CALL_TIMEOUT).proto(),
            ),
        })
    }

    pub async fn search(&self, query: &str, limit: u8) -> Result<Vec<SearchResultRow>, String> {
        let response = self
            .inner
            .search(SearchRequest {
                query: query.to_owned(),
                page_types: vec![],
                limit: u32::from(limit),
                ..Default::default()
            })
            .await
            .map_err(|e| e.to_string())?
            .into_owned();
        Ok(response
            .results
            .into_iter()
            .map(|result| SearchResultRow {
                title: result.title,
                snippet: result.snippet,
                source: result.document.as_ref().map(|d| d.source.clone()).unwrap_or_default(),
                source_id: result.document.as_ref().map(|d| d.source_id.clone()).unwrap_or_default(),
            })
            .collect())
    }

    pub async fn read_document(&self, source: &str, source_id: &str, version: &str) -> Result<String, String> {
        let response = self
            .inner
            .read_document(ReadDocumentRequest {
                document: Some(DocumentRef {
                    source: source.to_owned(),
                    source_id: source_id.to_owned(),
                    version: version.to_owned(),
                    ..Default::default()
                }),
                cursor: String::new(),
                max_chars: 0,
                ..Default::default()
            })
            .await
            .map_err(|e| e.to_string())?
            .into_owned();
        Ok(response.content)
    }
}
```

Test module — same fake-server shape as `services/knowledge-base/src/llm_router_client.rs`'s
`FakeLlmRouter`/`start_fake_llm_router`, but implementing `KnowledgeBaseService` instead of
`LlmRouterService`:
```rust
#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use common::proto::knowledge_base::v1::{
        IngestRequest, IngestResponse, KnowledgeBaseService, ReadDocumentResponse, SearchResponse,
        SearchResult,
    };
    use connectrpc::{RequestContext, Response, Router as ConnectRouter, ServiceRequest, ServiceResult};

    use super::*;

    struct FakeKnowledgeBase {
        received_queries: Mutex<Vec<String>>,
    }

    #[allow(refining_impl_trait)]
    impl KnowledgeBaseService for FakeKnowledgeBase {
        async fn search(
            &self,
            _ctx: RequestContext,
            request: ServiceRequest<'_, SearchRequest>,
        ) -> ServiceResult<SearchResponse> {
            let owned = request.to_owned_message();
            self.received_queries.lock().expect("lock").push(owned.query.clone());
            Response::ok(SearchResponse {
                results: vec![SearchResult {
                    source: "academy.claude.com".to_owned(),
                    source_id: "/courses/borrow-checker".to_owned(),
                    title: "The Borrow Checker".to_owned(),
                    snippet: "Borrowing lets code use a value without taking ownership.".to_owned(),
                    document: Some(DocumentRef {
                        source: "academy.claude.com".to_owned(),
                        source_id: "/courses/borrow-checker".to_owned(),
                        version: "abc123".to_owned(),
                        ..Default::default()
                    }),
                    ..Default::default()
                }],
                ..Default::default()
            })
        }

        async fn read_document(
            &self,
            _ctx: RequestContext,
            _request: ServiceRequest<'_, ReadDocumentRequest>,
        ) -> ServiceResult<ReadDocumentResponse> {
            Response::ok(ReadDocumentResponse {
                content: "Full document text.".to_owned(),
                ..Default::default()
            })
        }

        async fn ingest(
            &self,
            _ctx: RequestContext,
            _request: ServiceRequest<'_, IngestRequest>,
        ) -> ServiceResult<IngestResponse> {
            Response::ok(IngestResponse::default())
        }
    }

    async fn serve(fake: Arc<FakeKnowledgeBase>) -> String {
        let connect = ConnectRouter::new().add_service(fake);
        let app = axum::Router::new().fallback_service(connect.into_axum_service());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("addr");
        tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
        format!("http://{address}")
    }

    #[tokio::test]
    async fn search_sends_the_query_and_maps_results() {
        let fake = Arc::new(FakeKnowledgeBase { received_queries: Mutex::new(Vec::new()) });
        let url = serve(Arc::clone(&fake)).await;
        let client = KnowledgeBaseClient::new(&url).expect("client");

        let results = client.search("borrow checker", 5).await.expect("search");

        assert_eq!(*fake.received_queries.lock().expect("lock"), vec!["borrow checker".to_owned()]);
        assert_eq!(
            results,
            vec![SearchResultRow {
                title: "The Borrow Checker".to_owned(),
                snippet: "Borrowing lets code use a value without taking ownership.".to_owned(),
                source: "academy.claude.com".to_owned(),
                source_id: "/courses/borrow-checker".to_owned(),
            }]
        );
    }

    #[tokio::test]
    async fn read_document_returns_the_content() {
        let fake = Arc::new(FakeKnowledgeBase { received_queries: Mutex::new(Vec::new()) });
        let url = serve(fake).await;
        let client = KnowledgeBaseClient::new(&url).expect("client");

        let content = client.read_document("academy.claude.com", "/courses/borrow-checker", "abc123").await.expect("read");

        assert_eq!(content, "Full document text.");
    }
}
```

Add `common::proto::knowledge_base::v1::KnowledgeBaseServiceClient` — already generated, `tool`
becomes a new consumer of an existing contract, no `common` change needed.

- [ ] **Step 2: Run**

Run: `cargo test -p tool tools::kb_client:: 2>&1 | tail -30`
Expected: PASS, 1 test, once both `todo!()`/`unimplemented!()` placeholders are replaced with
real code (they must not survive into the commit — `clippy::todo`/`unimplemented` are
deny-level).

- [ ] **Step 3: Commit**

```bash
git add services/tool/src/tools/kb_client.rs services/tool/src/tools/mod.rs
git commit -m "tool: kb_search / kb_read_document — knowledge-base client"
```

Add `pub mod kb_client;` to `services/tool/src/tools/mod.rs` as part of this commit.

## Task 9: `error.rs` + `service.rs` — `CreateTool`, `ValidateTool`, `ActivateTool`, `ListTools`

**Files:**
- Create: `services/tool/src/error.rs`
- Create: `services/tool/src/service.rs`

**Interfaces:**
- Consumes: `entity::tool::{Model, ActiveModel, Entity, Risk, Status}` (Task 3).
- Produces: `ToolError` (thiserror enum, `From<ToolError> for ConnectError`); `Service { db:
  DatabaseConnection, llm: LlmRouterServiceClient<HttpClient> }` with `new(db, llm_router_url:
  &str) -> Result<Self, String>`, `create_tool(user_id: Option<Uuid>, slug, name, description,
  input_schema: Value, output_schema: Value, risk: Risk, timeout_seconds: i32) ->
  Result<i64, ToolError>`, `validate_tool(tool_id: i64, user_id: Uuid) -> Result<(bool, String),
  ToolError>`, `activate_tool(tool_id: i64, user_id: Uuid) -> Result<(), ToolError>`,
  `list_tools(user_id: Option<Uuid>) -> Result<Vec<Model>, ToolError>` — Task 10 adds `execute`
  to the same `impl Service`.

- [ ] **Step 1: `error.rs`**

```rust
// One error enum for the service, one place that maps it to Connect codes — same shape as
// services/engine/src/error.rs.

use connectrpc::ConnectError;

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("tool not found: {0}")]
    NotFound(i64),
    #[error("database error: {0}")]
    Db(#[from] sea_orm::DbErr),
    #[error("invalid json: {0}")]
    Json(#[from] serde_json::Error),
}

impl From<ToolError> for ConnectError {
    fn from(error: ToolError) -> Self {
        match &error {
            ToolError::InvalidRequest(_) => ConnectError::invalid_argument(error.to_string()),
            ToolError::NotFound(_) => ConnectError::not_found(error.to_string()),
            ToolError::Db(_) | ToolError::Json(_) => ConnectError::internal(error.to_string()),
        }
    }
}
```

Add `pub mod error;` to `services/tool/src/lib.rs`.

- [ ] **Step 2: Write the failing tests**

`services/tool/src/service.rs`:
```rust
// CreateTool/ValidateTool/ActivateTool/ListTools: the lifecycle a user-created tool goes
// through before it's callable — Execute (Task 10) is the only method that actually runs one.

use chrono::Utc;
use common::proto::llm_router::v1::{CompleteRequest, LlmRouterServiceClient, QualityTier, ResponseFormat, Sampling};
use connectrpc::Protocol;
use connectrpc::client::{ClientConfig, HttpClient};
use buffa::EnumValue;
use sea_orm::{ActiveModelTrait, ActiveValue::Set, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};
use serde_json::Value;
use std::time::Duration;
use uuid::Uuid;

use crate::entity::tool::{ActiveModel, Entity, Risk, Status};
use crate::error::ToolError;

const VALIDATE_TIMEOUT: Duration = Duration::from_secs(60);
const VALIDATE_SYSTEM_PROMPT: &str = r#"You validate a tool definition against 2026 API design standards. You will be given a tool's name, description, JSON Schema input_schema, JSON Schema output_schema, risk level, and timeout_seconds. Respond with exactly this JSON and nothing else: {"approved": true or false, "feedback": "one or two sentences"}. Approve only if input_schema and output_schema are each a plausible JSON Schema object, description clearly states what the tool does (and, for risk "write" or "destructive", what it changes), and timeout_seconds is between 1 and 300."#;

pub struct Service {
    db: DatabaseConnection,
    llm: LlmRouterServiceClient<HttpClient>,
}

impl Service {
    pub fn new(db: DatabaseConnection, llm_router_url: &str) -> Result<Self, String> {
        let target = llm_router_url
            .parse()
            .map_err(|e| format!("could not parse LLM_ROUTER_URL {llm_router_url:?}: {e}"))?;
        Ok(Self {
            db,
            llm: LlmRouterServiceClient::new(
                HttpClient::plaintext_http2_only(),
                ClientConfig::new(target).with_protocol(Protocol::Grpc).with_default_timeout(VALIDATE_TIMEOUT).proto(),
            ),
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn create_tool(
        &self,
        user_id: Option<Uuid>,
        slug: String,
        name: String,
        description: String,
        input_schema: Value,
        output_schema: Value,
        risk: Risk,
        timeout_seconds: i32,
    ) -> Result<i64, ToolError> {
        if !input_schema.is_object() || !output_schema.is_object() {
            return Err(ToolError::InvalidRequest("input_schema and output_schema must be JSON objects".to_owned()));
        }
        let now = Utc::now();
        let row = ActiveModel {
            user_id: Set(user_id),
            slug: Set(slug),
            name: Set(name),
            description: Set(description),
            input_schema: Set(input_schema),
            output_schema: Set(output_schema),
            connection_id: Set(None),
            risk: Set(risk),
            timeout_seconds: Set(timeout_seconds),
            status: Set(Status::Draft),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(&self.db)
        .await?;
        Ok(row.id)
    }

    pub async fn validate_tool(&self, tool_id: i64, user_id: Uuid) -> Result<(bool, String), ToolError> {
        let tool = self.owned_tool(tool_id, user_id).await?;
        let user_prompt = format!(
            "name: {}\ndescription: {}\ninput_schema: {}\noutput_schema: {}\nrisk: {:?}\ntimeout_seconds: {}",
            tool.name, tool.description, tool.input_schema, tool.output_schema, tool.risk, tool.timeout_seconds,
        );
        let response = self
            .llm
            .complete(CompleteRequest {
                tier: EnumValue::Known(QualityTier::Medium),
                system_prompt: VALIDATE_SYSTEM_PROMPT.to_owned(),
                user_prompt,
                sampling: Sampling { response_format: Some(EnumValue::Known(ResponseFormat::JsonObject)), ..Default::default() }.into(),
                ..Default::default()
            })
            .await
            .map_err(|e| ToolError::InvalidRequest(format!("llm-router call failed: {e}")))?
            .into_owned();
        let parsed: Value = serde_json::from_str(&response.content).unwrap_or(Value::Null);
        let approved = parsed.get("approved").and_then(Value::as_bool).unwrap_or(false);
        let feedback = parsed
            .get("feedback")
            .and_then(Value::as_str)
            .unwrap_or("the model did not return the expected JSON shape")
            .to_owned();

        let mut active: ActiveModel = tool.into();
        active.status = Set(if approved { Status::Validated } else { Status::Draft });
        active.updated_at = Set(Utc::now());
        active.update(&self.db).await?;

        Ok((approved, feedback))
    }

    pub async fn activate_tool(&self, tool_id: i64, user_id: Uuid) -> Result<(), ToolError> {
        let tool = self.owned_tool(tool_id, user_id).await?;
        if tool.status != Status::Validated {
            return Err(ToolError::InvalidRequest(format!("tool {tool_id} is not Validated")));
        }
        let mut active: ActiveModel = tool.into();
        active.status = Set(Status::Active);
        active.updated_at = Set(Utc::now());
        active.update(&self.db).await?;
        Ok(())
    }

    pub async fn list_tools(&self, user_id: Option<Uuid>) -> Result<Vec<crate::entity::tool::Model>, ToolError> {
        let mut query = Entity::find().filter(crate::entity::tool::Column::UserId.is_null());
        if let Some(id) = user_id {
            query = Entity::find().filter(
                crate::entity::tool::Column::UserId.is_null().or(crate::entity::tool::Column::UserId.eq(id)),
            );
        }
        Ok(query.all(&self.db).await?)
    }

    /// A tool the caller owns — used by `validate_tool`/`activate_tool`, which only ever act on
    /// the caller's own `Draft`/`Validated` tools (a system tool's `user_id` never matches any
    /// `Principal`, so this naturally rejects attempts to validate/activate one).
    async fn owned_tool(&self, tool_id: i64, user_id: Uuid) -> Result<crate::entity::tool::Model, ToolError> {
        Entity::find_by_id(tool_id)
            .filter(crate::entity::tool::Column::UserId.eq(user_id))
            .one(&self.db)
            .await?
            .ok_or(ToolError::NotFound(tool_id))
    }
}

#[cfg(all(test, feature = "test-support"))]
mod tests {
    use super::*;
    use common::proto::llm_router::v1::{CompleteResponse, DescribeTiersRequest, DescribeTiersResponse, LlmRouterService};
    use connectrpc::{RequestContext, Response, Router as ConnectRouter, ServiceRequest, ServiceResult};
    use serde_json::json;
    use std::sync::Arc;

    struct FakeLlmRouter { reply: String }

    #[allow(refining_impl_trait)]
    impl LlmRouterService for FakeLlmRouter {
        async fn complete(&self, _ctx: RequestContext, _request: ServiceRequest<'_, CompleteRequest>) -> ServiceResult<CompleteResponse> {
            Response::ok(CompleteResponse { content: self.reply.clone(), ..Default::default() })
        }
        async fn describe_tiers(&self, _ctx: RequestContext, _request: ServiceRequest<'_, DescribeTiersRequest>) -> ServiceResult<DescribeTiersResponse> {
            Response::ok(DescribeTiersResponse::default())
        }
    }

    async fn serve_llm(reply: &str) -> String {
        let fake = Arc::new(FakeLlmRouter { reply: reply.to_owned() });
        let connect = ConnectRouter::new().add_service(fake);
        let app = axum::Router::new().fallback_service(connect.into_axum_service());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("addr");
        tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
        format!("http://{address}")
    }

    async fn service_with(llm_url: &str) -> (crate::test_db::TestDb, Service) {
        let test = crate::test_db::start().await;
        let service = Service::new(test.db.clone(), llm_url).expect("service");
        (test, service)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn create_tool_starts_as_draft() {
        let llm_url = serve_llm(r#"{"approved": true, "feedback": "ok"}"#).await;
        let (test, service) = service_with(&llm_url).await;
        let user_id = Uuid::new_v4();

        let tool_id = service
            .create_tool(Some(user_id), "echo".into(), "Echo".into(), "Echoes input".into(), json!({}), json!({}), Risk::ReadOnly, 30)
            .await
            .expect("create");

        let row = crate::entity::tool::Entity::find_by_id(tool_id).one(&test.db).await.expect("query").expect("row");
        assert_eq!(row.status, Status::Draft);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn validate_tool_approves_and_advances_to_validated() {
        let llm_url = serve_llm(r#"{"approved": true, "feedback": "clear and safe"}"#).await;
        let (test, service) = service_with(&llm_url).await;
        let user_id = Uuid::new_v4();
        let tool_id = service
            .create_tool(Some(user_id), "echo".into(), "Echo".into(), "Echoes input".into(), json!({}), json!({}), Risk::ReadOnly, 30)
            .await
            .expect("create");

        let (approved, feedback) = service.validate_tool(tool_id, user_id).await.expect("validate");

        assert!(approved);
        assert_eq!(feedback, "clear and safe");
        let row = crate::entity::tool::Entity::find_by_id(tool_id).one(&test.db).await.expect("query").expect("row");
        assert_eq!(row.status, Status::Validated);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn validate_tool_rejection_stays_draft() {
        let llm_url = serve_llm(r#"{"approved": false, "feedback": "description is unclear"}"#).await;
        let (_test, service) = service_with(&llm_url).await;
        let user_id = Uuid::new_v4();
        let tool_id = service
            .create_tool(Some(user_id), "echo".into(), "Echo".into(), "".into(), json!({}), json!({}), Risk::ReadOnly, 30)
            .await
            .expect("create");

        let (approved, _) = service.validate_tool(tool_id, user_id).await.expect("validate");

        assert!(!approved);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn activate_tool_requires_validated_status() {
        let llm_url = serve_llm(r#"{"approved": true, "feedback": "ok"}"#).await;
        let (_test, service) = service_with(&llm_url).await;
        let user_id = Uuid::new_v4();
        let tool_id = service
            .create_tool(Some(user_id), "echo".into(), "Echo".into(), "Echoes input".into(), json!({}), json!({}), Risk::ReadOnly, 30)
            .await
            .expect("create");

        assert!(service.activate_tool(tool_id, user_id).await.is_err(), "still Draft, not Validated");

        service.validate_tool(tool_id, user_id).await.expect("validate");
        service.activate_tool(tool_id, user_id).await.expect("activate");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_tools_hides_other_users_tools() {
        let llm_url = serve_llm(r#"{"approved": true, "feedback": "ok"}"#).await;
        let (_test, service) = service_with(&llm_url).await;
        let owner = Uuid::new_v4();
        let stranger = Uuid::new_v4();
        service
            .create_tool(Some(owner), "mine".into(), "Mine".into(), "d".into(), json!({}), json!({}), Risk::ReadOnly, 30)
            .await
            .expect("create");

        let seen_by_stranger = service.list_tools(Some(stranger)).await.expect("list");

        assert!(seen_by_stranger.iter().all(|t| t.slug != "mine"));
    }
}
```

Add `pub mod service;` to `services/tool/src/lib.rs`. Add `services/tool/src/test_db.rs`
(feature-gated `test-support`, `ServiceSchema { schema: "tool", role: "tool_user", password_var:
"TOOL_DB_PASSWORD", entity_prefix: "tool::entity::*" }`, calling
`crate::entity::tool::INDEX_STATEMENTS_CREATED_AFTER_SCHEMA_SYNC` after sync — copy
`services/engine/src/test_db.rs`'s exact shape) and `#[cfg(feature = "test-support")] pub mod
test_db;` to `lib.rs`.

- [ ] **Step 3: Run**

Run: `cargo test -p tool --features test-support service:: 2>&1 | tail -50`
Requires Docker (testcontainers). Expected: PASS, 5 tests.

- [ ] **Step 4: Commit**

```bash
git add services/tool/src/error.rs services/tool/src/service.rs services/tool/src/test_db.rs services/tool/src/lib.rs
git commit -m "tool: service.rs — CreateTool, ValidateTool, ActivateTool, ListTools"
```

## Task 10: `Execute` — dispatch to system tools, policy check

**Files:**
- Create: `services/tool/src/slugs.rs`
- Modify: `services/tool/src/service.rs` — extend `Service` struct + `impl Service`

**Interfaces:**
- Consumes: `providers::brave::BraveSearchProvider` (Task 6), `tools::web_fetch::fetch` (Task 7),
  `tools::kb_client::KnowledgeBaseClient` (Task 8), `policy::check_policy` (Task 5).
- Produces: `slugs::{WEB_SEARCH, WEB_FETCH, KB_SEARCH, KB_READ_DOCUMENT}: &str`; extends
  `Service::new` to `new(db, llm_router_url, brave_api_key: String, knowledge_base_url: &str) ->
  Result<Self, String>`; adds `enum ExecuteOutcome { Ok(Value), RequiresApproval,
  NotExecutable(String), Error(String) }` and `async fn execute(&self, caller: Option<Uuid>,
  slug: &str, input_json: &str, idempotency_key: &str) -> Result<ExecuteOutcome, ToolError>` —
  Task 13's `ToolTaskExecutor` calls this indirectly through the `Execute` RPC (Task 11), not
  directly (it's a different process); this signature is what the RPC handler wraps.

- [ ] **Step 1: `slugs.rs`**

```rust
// Slugs of the four system tools this service seeds at startup (Task 11) and dispatches Execute
// to by name (Task 10). A slug not in this list belongs to a user-created tool, which today has
// no runnable implementation — see the spec's "Вне скоупа".

pub const WEB_SEARCH: &str = "web_search";
pub const WEB_FETCH: &str = "web_fetch";
pub const KB_SEARCH: &str = "kb_search";
pub const KB_READ_DOCUMENT: &str = "kb_read_document";
```

Add `pub mod slugs;` to `services/tool/src/lib.rs`.

- [ ] **Step 2: Write the failing tests**

Add to `services/tool/src/service.rs`, extending the existing `Service` struct and `impl`:
```rust
use crate::providers::brave::BraveSearchProvider;
use crate::tools::kb_client::KnowledgeBaseClient;

pub struct Service {
    db: DatabaseConnection,
    llm: LlmRouterServiceClient<HttpClient>,
    search: BraveSearchProvider,
    http: reqwest::Client,
    kb: KnowledgeBaseClient,
}

impl Service {
    pub fn new(
        db: DatabaseConnection,
        llm_router_url: &str,
        brave_api_key: String,
        knowledge_base_url: &str,
    ) -> Result<Self, String> {
        let target = llm_router_url
            .parse()
            .map_err(|e| format!("could not parse LLM_ROUTER_URL {llm_router_url:?}: {e}"))?;
        Ok(Self {
            db,
            llm: LlmRouterServiceClient::new(
                HttpClient::plaintext_http2_only(),
                ClientConfig::new(target).with_protocol(Protocol::Grpc).with_default_timeout(VALIDATE_TIMEOUT).proto(),
            ),
            search: BraveSearchProvider::new(brave_api_key),
            http: reqwest::Client::new(),
            kb: KnowledgeBaseClient::new(knowledge_base_url)?,
        })
    }

    // ... create_tool / validate_tool / activate_tool / list_tools / owned_tool unchanged ...

    pub async fn execute(
        &self,
        caller: Option<Uuid>,
        slug: &str,
        input_json: &str,
        idempotency_key: &str,
    ) -> Result<ExecuteOutcome, ToolError> {
        tracing::debug!(idempotency_key, slug, "tool execute (key logged, not enforced — no destructive side effect here yet, see the spec's Порты-equivalent note)");
        let Some(tool) = self.find_tool(caller, slug).await? else {
            return Ok(ExecuteOutcome::Error(format!("no tool with slug {slug:?}")));
        };
        if crate::policy::check_policy(tool.risk) == crate::policy::PolicyDecision::RequiresApproval {
            return Ok(ExecuteOutcome::RequiresApproval);
        }
        let input: Value = serde_json::from_str(input_json).map_err(|e| ToolError::InvalidRequest(e.to_string()))?;
        self.run_system_tool(slug, &input).await
    }

    async fn find_tool(&self, caller: Option<Uuid>, slug: &str) -> Result<Option<crate::entity::tool::Model>, ToolError> {
        if let Some(user_id) = caller {
            let own = Entity::find()
                .filter(crate::entity::tool::Column::UserId.eq(user_id))
                .filter(crate::entity::tool::Column::Slug.eq(slug))
                .one(&self.db)
                .await?;
            if own.is_some() {
                return Ok(own);
            }
        }
        Ok(Entity::find()
            .filter(crate::entity::tool::Column::UserId.is_null())
            .filter(crate::entity::tool::Column::Slug.eq(slug))
            .one(&self.db)
            .await?)
    }

    async fn run_system_tool(&self, slug: &str, input: &Value) -> Result<ExecuteOutcome, ToolError> {
        match slug {
            crate::slugs::WEB_SEARCH => {
                let query = input.get("query").and_then(Value::as_str).unwrap_or_default();
                let limit = input.get("limit").and_then(Value::as_u64).map_or(5, |v| u8::try_from(v.min(20)).unwrap_or(20));
                Ok(match self.search.search(query, limit).await {
                    Ok(results) => ExecuteOutcome::Ok(serde_json::to_value(results)?),
                    Err(error) => ExecuteOutcome::Error(error),
                })
            }
            crate::slugs::WEB_FETCH => {
                let url = input.get("url").and_then(Value::as_str).unwrap_or_default();
                Ok(match crate::tools::web_fetch::fetch(&self.http, url).await {
                    Ok(page) => ExecuteOutcome::Ok(serde_json::to_value(page)?),
                    Err(error) => ExecuteOutcome::Error(error),
                })
            }
            crate::slugs::KB_SEARCH => {
                let query = input.get("query").and_then(Value::as_str).unwrap_or_default();
                let limit = input.get("limit").and_then(Value::as_u64).map_or(5, |v| u8::try_from(v.min(20)).unwrap_or(20));
                Ok(match self.kb.search(query, limit).await {
                    Ok(results) => ExecuteOutcome::Ok(serde_json::to_value(results)?),
                    Err(error) => ExecuteOutcome::Error(error),
                })
            }
            crate::slugs::KB_READ_DOCUMENT => {
                let source = input.get("source").and_then(Value::as_str).unwrap_or_default();
                let source_id = input.get("source_id").and_then(Value::as_str).unwrap_or_default();
                let version = input.get("version").and_then(Value::as_str).unwrap_or_default();
                Ok(match self.kb.read_document(source, source_id, version).await {
                    Ok(content) => ExecuteOutcome::Ok(serde_json::json!({"content": content})),
                    Err(error) => ExecuteOutcome::Error(error),
                })
            }
            other => Ok(ExecuteOutcome::NotExecutable(format!(
                "tool {other:?} has no runnable implementation yet — user-created tools need Integrations Service"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ExecuteOutcome {
    Ok(Value),
    RequiresApproval,
    NotExecutable(String),
    Error(String),
}
```

`Service::new` growing to 4 required parameters breaks Task 9's `service_with` helper, which
still calls the old 2-arg form — update it in place so every one of Task 9's existing call sites
(`service_with(&llm_url)`) keeps compiling unchanged. Its tests never exercise `web_search` or
`kb_search`, so a Brave key and a knowledge-base URL that are never dereferenced are enough:

```rust
    async fn service_with(llm_url: &str) -> (crate::test_db::TestDb, Service) {
        let test = crate::test_db::start().await;
        let service = Service::new(test.db.clone(), llm_url, "unused-in-these-tests".to_owned(), "http://127.0.0.1:1")
            .expect("service");
        (test, service)
    }
```

Task 10's own new tests need a knowledge-base server that actually answers `kb_search`, so they
use a second, separate helper instead of overloading `service_with`:

```rust
    async fn full_service_with(llm_url: &str, kb_url: &str) -> (crate::test_db::TestDb, Service) {
        let test = crate::test_db::start().await;
        let service = Service::new(test.db.clone(), llm_url, "unused-in-these-tests".to_owned(), kb_url).expect("service");
        (test, service)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn execute_on_a_read_only_system_tool_runs_it() {
        let llm_url = serve_llm(r#"{"approved": true, "feedback": "ok"}"#).await;
        let kb_url = crate::tools::kb_client::tests_support::serve_kb().await; // see note below
        let (_test, service) = full_service_with(&llm_url, &kb_url).await;
        seed_system_tools(&service).await;

        let outcome = service.execute(None, crate::slugs::KB_SEARCH, r#"{"query": "borrow checker"}"#, "e:kb_search:0").await.expect("execute");

        assert!(matches!(outcome, ExecuteOutcome::Ok(_)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn execute_on_an_unregistered_slug_is_an_error() {
        let llm_url = serve_llm(r#"{"approved": true, "feedback": "ok"}"#).await;
        let kb_url = crate::tools::kb_client::tests_support::serve_kb().await;
        let (_test, service) = full_service_with(&llm_url, &kb_url).await;

        let outcome = service.execute(None, "nonexistent", "{}", "e:x:0").await.expect("execute");

        assert!(matches!(outcome, ExecuteOutcome::Error(_)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn execute_on_a_write_tool_requires_approval_without_running_it() {
        let llm_url = serve_llm(r#"{"approved": true, "feedback": "ok"}"#).await;
        let kb_url = crate::tools::kb_client::tests_support::serve_kb().await;
        let (_test, service) = full_service_with(&llm_url, &kb_url).await;
        let user_id = Uuid::new_v4();
        service
            .create_tool(Some(user_id), "send_email".into(), "Send Email".into(), "Sends an email".into(), json!({}), json!({}), Risk::Write, 30)
            .await
            .expect("create");

        let outcome = service.execute(Some(user_id), "send_email", "{}", "e:mail:0").await.expect("execute");

        assert_eq!(outcome, ExecuteOutcome::RequiresApproval);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn execute_on_a_user_created_tool_is_not_executable() {
        let llm_url = serve_llm(r#"{"approved": true, "feedback": "ok"}"#).await;
        let kb_url = crate::tools::kb_client::tests_support::serve_kb().await;
        let (_test, service) = full_service_with(&llm_url, &kb_url).await;
        let user_id = Uuid::new_v4();
        service
            .create_tool(Some(user_id), "custom_thing".into(), "Custom".into(), "d".into(), json!({}), json!({}), Risk::ReadOnly, 30)
            .await
            .expect("create");

        let outcome = service.execute(Some(user_id), "custom_thing", "{}", "e:c:0").await.expect("execute");

        assert!(matches!(outcome, ExecuteOutcome::NotExecutable(_)));
    }

    async fn seed_system_tools(service: &Service) {
        // Mirrors Task 11's real seeding, scoped to just what Task 10's tests need.
        service
            .create_tool(
                None, // system tool — create_tool takes Option<Uuid> exactly so this is expressible
                crate::slugs::KB_SEARCH.into(), "Knowledge base search".into(), "Searches the knowledge base".into(),
                json!({"type": "object"}), json!({"type": "object"}), Risk::ReadOnly, 30,
            )
            .await
            .expect("seed kb_search");
    }
```

Add a `tests_support` module to `services/tool/src/tools/kb_client.rs` (behind
`#[cfg(feature = "test-support")]`, not `#[cfg(test)]` — Task 10's tests live in a different
module and need to call it) exposing the existing `serve` helper from Task 8's test module under
a public name:
```rust
#[cfg(feature = "test-support")]
pub mod tests_support {
    use std::sync::{Arc, Mutex};
    use common::proto::knowledge_base::v1::{
        DocumentRef, IngestRequest, IngestResponse, KnowledgeBaseService, ReadDocumentRequest,
        ReadDocumentResponse, SearchRequest, SearchResponse, SearchResult,
    };
    use connectrpc::{RequestContext, Response, Router as ConnectRouter, ServiceRequest, ServiceResult};

    struct FakeKnowledgeBase;

    #[allow(refining_impl_trait)]
    impl KnowledgeBaseService for FakeKnowledgeBase {
        async fn search(&self, _ctx: RequestContext, _request: ServiceRequest<'_, SearchRequest>) -> ServiceResult<SearchResponse> {
            Response::ok(SearchResponse {
                results: vec![SearchResult {
                    source: "academy.claude.com".to_owned(),
                    source_id: "/courses/x".to_owned(),
                    title: "X".to_owned(),
                    snippet: "x".to_owned(),
                    document: Some(DocumentRef { source: "academy.claude.com".to_owned(), source_id: "/courses/x".to_owned(), version: "v1".to_owned(), ..Default::default() }),
                    ..Default::default()
                }],
                ..Default::default()
            })
        }
        async fn read_document(&self, _ctx: RequestContext, _request: ServiceRequest<'_, ReadDocumentRequest>) -> ServiceResult<ReadDocumentResponse> {
            Response::ok(ReadDocumentResponse { content: "content".to_owned(), ..Default::default() })
        }
        async fn ingest(&self, _ctx: RequestContext, _request: ServiceRequest<'_, IngestRequest>) -> ServiceResult<IngestResponse> {
            Response::ok(IngestResponse::default())
        }
    }

    /// A fake knowledge-base server for tests outside this module that just need `Service::new`
    /// to have somewhere to point — not a source of test assertions itself (Task 8's own tests
    /// already cover mapping correctness).
    pub async fn serve_kb() -> String {
        let connect = ConnectRouter::new().add_service(Arc::new(FakeKnowledgeBase));
        let app = axum::Router::new().fallback_service(connect.into_axum_service());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("addr");
        tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
        format!("http://{address}")
    }
}
```

- [ ] **Step 3: Run**

Run: `cargo test -p tool --features test-support service:: 2>&1 | tail -60`
Expected: PASS, 9 tests (5 from Task 9 + 4 new).

- [ ] **Step 4: Commit**

```bash
git add services/tool/src/slugs.rs services/tool/src/service.rs services/tool/src/tools/kb_client.rs services/tool/src/lib.rs
git commit -m "tool: Execute — dispatch to system tools, enforce risk policy"
```

## Task 11: `main.rs` — Connect trait impl, system-tool seeding, the running service

**Files:**
- Modify: `services/tool/src/main.rs` (replaces the Task 3 stub entirely)

**Interfaces:**
- Consumes: everything built in Tasks 3-10.
- Produces: a running binary on port `8087` — no new public Rust API, this is the assembly
  point. Verified by Task 12's `docker compose up` + curl, not a unit test.

- [ ] **Step 1: Write `main.rs`**

```rust
// tool: the tool registry + executor service. Connects to Postgres, syncs its schema, applies
// the manual uniqueness index, seeds the four system tools if they don't already exist, serves
// Connect RPC.

use std::sync::Arc;

use axum::routing::get;
use common::proto::tools::v1::{
    ActivateToolRequest, ActivateToolResponse, CreateToolRequest, CreateToolResponse,
    ExecuteRequest, ExecuteResponse, ListToolsRequest, ListToolsResponse, Tool as ToolProto,
    ToolService, ValidateToolRequest, ValidateToolResponse,
};
use connectrpc::{ConnectError, RequestContext, Response, Router as ConnectRouter, ServiceRequest, ServiceResult};
use sea_orm::{ColumnTrait, ConnectionTrait, Database, EntityTrait, QueryFilter};
use uuid::Uuid;

use tool::entity::tool::{Entity, Risk, Status, INDEX_STATEMENTS_CREATED_AFTER_SCHEMA_SYNC};
use tool::service::{ExecuteOutcome, Service};
use tool::slugs;

const TOOL_PORT: &str = "0.0.0.0:8087";

fn env(name: &str) -> Result<String, String> {
    std::env::var(name).map_err(|_| format!("{name} is not set"))
}

fn parse_uuid(value: &str, field: &str) -> Result<Uuid, ConnectError> {
    Uuid::parse_str(value).map_err(|_| ConnectError::invalid_argument(format!("{field} is not a valid uuid: {value}")))
}

fn risk_from_str(value: &str) -> Result<Risk, ConnectError> {
    match value {
        "read_only" => Ok(Risk::ReadOnly),
        "write" => Ok(Risk::Write),
        "destructive" => Ok(Risk::Destructive),
        other => Err(ConnectError::invalid_argument(format!("unknown risk {other:?}"))),
    }
}

fn risk_to_str(risk: Risk) -> &'static str {
    match risk {
        Risk::ReadOnly => "read_only",
        Risk::Write => "write",
        Risk::Destructive => "destructive",
    }
}

fn status_to_str(status: Status) -> &'static str {
    match status {
        Status::Draft => "draft",
        Status::Validated => "validated",
        Status::Active => "active",
    }
}

struct ToolServiceImpl {
    service: Service,
}

#[allow(refining_impl_trait)]
impl ToolService for ToolServiceImpl {
    async fn create_tool(&self, ctx: RequestContext, request: ServiceRequest<'_, CreateToolRequest>) -> ServiceResult<CreateToolResponse> {
        let msg = request.to_owned_message();
        let Some(principal) = common::principal::from_metadata(ctx.headers()) else {
            return Err(ConnectError::invalid_argument("CreateTool needs a Principal in the request metadata"));
        };
        let input_schema = serde_json::from_str(&msg.input_schema_json).map_err(|e| ConnectError::invalid_argument(e.to_string()))?;
        let output_schema = serde_json::from_str(&msg.output_schema_json).map_err(|e| ConnectError::invalid_argument(e.to_string()))?;
        let risk = risk_from_str(&msg.risk)?;
        let tool_id = self
            .service
            .create_tool(Some(principal.user_id), msg.slug, msg.name, msg.description, input_schema, output_schema, risk, msg.timeout_seconds)
            .await?;
        Response::ok(CreateToolResponse { tool_id: tool_id.to_string(), ..Default::default() })
    }

    async fn validate_tool(&self, ctx: RequestContext, request: ServiceRequest<'_, ValidateToolRequest>) -> ServiceResult<ValidateToolResponse> {
        let msg = request.to_owned_message();
        let Some(principal) = common::principal::from_metadata(ctx.headers()) else {
            return Err(ConnectError::invalid_argument("ValidateTool needs a Principal in the request metadata"));
        };
        let tool_id: i64 = msg.tool_id.parse().map_err(|_| ConnectError::invalid_argument("tool_id is not a valid id"))?;
        let (approved, feedback) = self.service.validate_tool(tool_id, principal.user_id).await?;
        Response::ok(ValidateToolResponse { approved, feedback, ..Default::default() })
    }

    async fn activate_tool(&self, ctx: RequestContext, request: ServiceRequest<'_, ActivateToolRequest>) -> ServiceResult<ActivateToolResponse> {
        let msg = request.to_owned_message();
        let Some(principal) = common::principal::from_metadata(ctx.headers()) else {
            return Err(ConnectError::invalid_argument("ActivateTool needs a Principal in the request metadata"));
        };
        let tool_id: i64 = msg.tool_id.parse().map_err(|_| ConnectError::invalid_argument("tool_id is not a valid id"))?;
        self.service.activate_tool(tool_id, principal.user_id).await?;
        Response::ok(ActivateToolResponse::default())
    }

    async fn list_tools(&self, ctx: RequestContext, _request: ServiceRequest<'_, ListToolsRequest>) -> ServiceResult<ListToolsResponse> {
        let user_id = common::principal::from_metadata(ctx.headers()).map(|p| p.user_id);
        let tools = self.service.list_tools(user_id).await?;
        Response::ok(ListToolsResponse {
            tools: tools
                .into_iter()
                .map(|t| ToolProto {
                    id: t.id.to_string(),
                    slug: t.slug,
                    name: t.name,
                    description: t.description,
                    input_schema_json: t.input_schema.to_string(),
                    risk: risk_to_str(t.risk).to_owned(),
                    status: status_to_str(t.status).to_owned(),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        })
    }

    async fn execute(&self, ctx: RequestContext, request: ServiceRequest<'_, ExecuteRequest>) -> ServiceResult<ExecuteResponse> {
        let msg = request.to_owned_message();
        let caller = common::principal::from_metadata(ctx.headers()).map(|p| p.user_id);
        let outcome = self.service.execute(caller, &msg.slug, &msg.input_json, &msg.idempotency_key).await?;
        Response::ok(match outcome {
            ExecuteOutcome::Ok(value) => ExecuteResponse { status: "ok".to_owned(), output_json: value.to_string(), ..Default::default() },
            ExecuteOutcome::RequiresApproval => ExecuteResponse { status: "requires_approval".to_owned(), ..Default::default() },
            ExecuteOutcome::NotExecutable(message) => ExecuteResponse { status: "not_executable".to_owned(), error_message: message, ..Default::default() },
            ExecuteOutcome::Error(message) => ExecuteResponse { status: "error".to_owned(), error_message: message, ..Default::default() },
        })
    }
}

async fn seed_system_tools(service: &Service, db: &sea_orm::DatabaseConnection) {
    let system_tools = [
        (slugs::WEB_SEARCH, "Web search", "Searches the web via Brave Search and returns matching pages.", Risk::ReadOnly),
        (slugs::WEB_FETCH, "Fetch a web page", "Fetches a URL and returns its readable title and text.", Risk::ReadOnly),
        (slugs::KB_SEARCH, "Knowledge base search", "Searches this project's knowledge base.", Risk::ReadOnly),
        (slugs::KB_READ_DOCUMENT, "Read a knowledge base document", "Reads the full text of one knowledge base document.", Risk::ReadOnly),
    ];
    for (slug, name, description, risk) in system_tools {
        let exists = Entity::find()
            .filter(tool::entity::tool::Column::UserId.is_null())
            .filter(tool::entity::tool::Column::Slug.eq(slug))
            .one(db)
            .await
            .unwrap_or(None)
            .is_some();
        if exists {
            continue;
        }
        match service
            .create_tool(None, slug.to_owned(), name.to_owned(), description.to_owned(), serde_json::json!({"type": "object"}), serde_json::json!({"type": "object"}), risk, 30)
            .await
        {
            Ok(_) => tracing::info!(slug, "seeded system tool"),
            Err(error) => tracing::error!(slug, %error, "failed to seed system tool"),
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    common::logging::init();

    let database_url = env("DATABASE_URL")?;
    let db = Database::connect(&database_url).await?;
    db.get_schema_registry("tool::entity::*").sync(&db).await?;
    for statement in INDEX_STATEMENTS_CREATED_AFTER_SCHEMA_SYNC {
        db.execute_unprepared(statement).await?;
    }

    let service = Service::new(
        db.clone(),
        &env("LLM_ROUTER_URL")?,
        std::env::var("BRAVE_SEARCH_API_KEY").unwrap_or_default(),
        &env("KNOWLEDGE_BASE_URL")?,
    )?;
    seed_system_tools(&service, &db).await;

    let tool_service = ToolServiceImpl { service };
    let connect = ConnectRouter::new().add_service(Arc::new(tool_service));
    let app = axum::Router::new()
        .route("/health", get(|| async { "OK" }))
        .fallback_service(connect.into_axum_service());

    let listener = tokio::net::TcpListener::bind(TOOL_PORT).await?;
    tracing::info!("tool listening on {TOOL_PORT}");
    axum::serve(listener, app).await?;
    Ok(())
}
```

`seed_system_tools`'s existence check (plain `SELECT` then `INSERT`) is not race-proof against
two `tool` instances both cold-starting at the exact same instant on a brand-new database — a
narrow window that would surface as one instance's `create_tool` failing on the unique index and
logging an error, self-correcting on that instance's next restart. Accepted for this scope, same
spirit as the rest of this plan's pragmatic choices; note it in the final report rather than
building retry logic for it.

- [ ] **Step 2: Verify it compiles**

Run: `cargo check -p tool --all-features 2>&1 | tail -30`
Expected: compiles clean. `BRAVE_SEARCH_API_KEY` is read via `std::env::var(...).unwrap_or_default()`,
not the `env()` helper that errors on missing — deliberately optional per the spec ("без него тул
... вернёт ошибку авторизации", not a startup failure).

- [ ] **Step 3: Commit**

```bash
git add services/tool/src/main.rs
git commit -m "tool: main.rs — Connect trait impl, system-tool seeding, wire it all up"
```

## Task 12: Docker — Dockerfile, compose.yaml, Postgres role/schema, `.env.example`

**Files:**
- Create: `services/tool/Dockerfile`
- Modify: `compose.yaml`, `.env.example`

**Interfaces:** none — this task makes the service buildable, runnable in the stack, and proves
it with a real health check; the end-to-end proof of what it actually *does* is Task 15.

- [ ] **Step 1: `Dockerfile`, mirroring `services/engine/Dockerfile`**

`services/tool/Dockerfile`:
```dockerfile
# Tool image: compiles the tool registry/executor with cached dependencies and ships the static binary alone on Alpine.
FROM rust:1.98-alpine3.24 AS build
RUN apk add --no-cache protoc
WORKDIR /app
ENV CARGO_HOME=/var/cache/cargo CARGO_TARGET_DIR=/var/cache/target
RUN --mount=type=bind,target=. \
    --mount=type=cache,target=/var/cache/cargo \
    --mount=type=cache,target=/var/cache/target \
    cargo build --locked --release --package tool \
    && cp /var/cache/target/release/tool /usr/local/bin/

FROM alpine:3.24
COPY --from=build /usr/local/bin/tool /usr/local/bin/
USER 10001:10001
EXPOSE 8087
HEALTHCHECK --interval=5s CMD ["wget", "-qO-", "http://127.0.0.1:8087/health"]
CMD ["tool"]
```

- [ ] **Step 2: `compose.yaml` — Postgres role/schema**

Add to the `postgres-bootstrap` config's `content`, after the existing `engine_user` block:
```
      \getenv tool_password TOOL_DB_PASSWORD
      CREATE ROLE tool_user LOGIN PASSWORD :'tool_password';
      CREATE SCHEMA tool AUTHORIZATION tool_user;
      ALTER ROLE tool_user SET search_path TO tool, public;
```
Add to the `postgres` service's `environment` block, alongside the existing `*_DB_PASSWORD`
lines:
```yaml
      TOOL_DB_PASSWORD: ${TOOL_DB_PASSWORD:?set it in .env, see .env.example}
```

- [ ] **Step 3: `compose.yaml` — the `tool` service block**

```yaml
  tool:
    build:
      dockerfile: services/tool/Dockerfile
    develop: *rebuild-on-save
    environment:
      LLM_ROUTER_URL: http://llm-router:8083
      KNOWLEDGE_BASE_URL: http://knowledge-base:8084
      BRAVE_SEARCH_API_KEY: ${BRAVE_SEARCH_API_KEY:-}
      DATABASE_URL: postgres://tool_user:${TOOL_DB_PASSWORD:?set it in .env, see .env.example}@postgres:5432/${POSTGRES_DB:-app}
    depends_on:
      postgres:
        condition: service_healthy
      llm-router:
        condition: service_healthy
      knowledge-base:
        condition: service_healthy
    init: true
    restart: unless-stopped
    cpus: 0.5
    mem_limit: 128m
    memswap_limit: 128m
```

- [ ] **Step 4: `.env.example`**

Add, in the Postgres section alongside the existing `*_DB_PASSWORD` lines:
```
TOOL_DB_PASSWORD=change-me
```
Add, near the existing provider-key comments (e.g. `OPENROUTER_API_KEY`):
```
# tool: Brave Search API key for the web_search system tool — https://api.search.brave.com.
# Optional: without it, every tool works except web_search, which returns an auth error when called.
BRAVE_SEARCH_API_KEY=
```

- [ ] **Step 5: Bootstrap the live Postgres role/schema and bring the service up**

The bootstrap config in `compose.yaml` only runs against a fresh volume — this stack's Postgres
has been running for days, so create the role/schema by hand, the same way `engine_user`/`engine`
were created earlier today:
```bash
PW=$(openssl rand -hex 24)
printf '\n# tool: registry + executor service (services/tool)\nTOOL_DB_PASSWORD=%s\n' "$PW" >> .env
docker compose exec -T -e PGPASSWORD="$(grep '^POSTGRES_PASSWORD=' .env | cut -d= -f2-)" postgres \
  psql -U "$(grep '^POSTGRES_USER=' .env | cut -d= -f2-)" -d "$(grep '^POSTGRES_DB=' .env | cut -d= -f2-)" \
  -v ON_ERROR_STOP=1 -v pw="$PW" <<'SQL'
DO $$ BEGIN IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname='tool_user') THEN CREATE ROLE tool_user LOGIN; END IF; END $$;
ALTER ROLE tool_user LOGIN PASSWORD :'pw';
CREATE SCHEMA IF NOT EXISTS tool AUTHORIZATION tool_user;
ALTER ROLE tool_user SET search_path TO tool, public;
SQL
```
Then:
```bash
docker compose -p aiengineeringboilerplate build tool
docker compose -p aiengineeringboilerplate up -d --no-deps tool
docker compose -p aiengineeringboilerplate ps --format '{{.Service}} {{.Status}}'
docker exec aiengineeringboilerplate-tool-1 wget -qO- http://127.0.0.1:8087/health
```
Expected: `tool` shows `Up`/`healthy` alongside the other 6 services (7 total now); health check
returns `OK`; `docker logs aiengineeringboilerplate-tool-1` shows 4 `"seeded system tool"` lines
(or none, if re-run — idempotent per Task 11).

If `docker compose build`/`up` default to the wrong Compose project name (this bit a previous
pass on this stack — a bare `docker compose` picks up the current directory's basename as the
project unless `-p aiengineeringboilerplate` is given explicitly, or unless a `.env`/`COMPOSE_PROJECT_NAME`
already pins it), always pass `-p aiengineeringboilerplate` explicitly as shown above.

- [ ] **Step 6: Commit**

```bash
git add services/tool/Dockerfile compose.yaml .env.example
git commit -m "tool: Docker — Dockerfile, compose service, Postgres role/schema, .env.example"
```

## Task 13: `services/engine` — `ToolTaskExecutor`

**Files:**
- Create: `services/engine/src/executors/tool.rs`
- Modify: `services/engine/src/executors/mod.rs` (add `pub mod tool;`)

**Interfaces:**
- Consumes: `common::proto::tools::v1::{ToolServiceClient, ExecuteRequest, ListToolsRequest,
  Tool}` (Task 4); `engine_core::{TaskExecutor, TaskError}` (already in `services/engine`).
- Produces: `struct ToolTaskExecutor { client: ToolServiceClient<HttpClient> }` with `fn
  new(tool_service_url: &str) -> Result<Self, String>`, `async fn list_tools(&self) ->
  Result<Vec<common::proto::tools::v1::Tool>, String>` (used by Task 14's `Dispatcher`), and
  `impl TaskExecutor` reading the tool call from `state["llm"]["tool_call"]` per `agent_graph`'s
  convention (`engine-core/src/builder.rs`).

- [ ] **Step 1: Write the failing tests — fake `ToolService`, mirroring `executors/llm.rs`'s Task 14 pattern**

`services/engine/src/executors/tool.rs`:
```rust
// TaskExecutor for kind="tool": calls Tool Service's Execute, reading the call itself from
// state["llm"]["tool_call"] (agent_graph's own convention, not from `config` — see
// engine-core/src/builder.rs's doc comment on the "tool" node).

use common::proto::tools::v1::{ExecuteRequest, ListToolsRequest, Tool, ToolServiceClient};
use connectrpc::Protocol;
use connectrpc::client::{ClientConfig, HttpClient};
use engine_core::{TaskError, TaskExecutor};
use serde_json::Value;
use std::time::Duration;

const CALL_TIMEOUT: Duration = Duration::from_secs(60);

pub struct ToolTaskExecutor {
    client: ToolServiceClient<HttpClient>,
}

impl ToolTaskExecutor {
    pub fn new(tool_service_url: &str) -> Result<Self, String> {
        let target = tool_service_url
            .parse()
            .map_err(|e| format!("could not parse TOOL_SERVICE_URL {tool_service_url:?}: {e}"))?;
        Ok(Self {
            client: ToolServiceClient::new(
                HttpClient::plaintext_http2_only(),
                ClientConfig::new(target).with_protocol(Protocol::Grpc).with_default_timeout(CALL_TIMEOUT).proto(),
            ),
        })
    }

    /// The catalog `Dispatcher` (Task 14) injects into an LLM node's `config` before a
    /// tool-calling turn — a plain RPC, not part of the `TaskExecutor` port.
    pub async fn list_tools(&self) -> Result<Vec<Tool>, String> {
        let response = self.client.list_tools(ListToolsRequest::default()).await.map_err(|e| e.to_string())?.into_owned();
        Ok(response.tools)
    }
}

impl TaskExecutor for ToolTaskExecutor {
    async fn execute(&self, kind: &str, _config: &Value, state: &Value, idempotency_key: &str) -> Result<Value, TaskError> {
        debug_assert_eq!(kind, "tool");
        let call = state
            .pointer("/llm/tool_call")
            .ok_or_else(|| TaskError::Failed("no tool_call at state[\"llm\"][\"tool_call\"]".to_owned()))?;
        let slug = call
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| TaskError::Failed("tool_call.name is missing".to_owned()))?;
        let args = call.get("args").cloned().unwrap_or_else(|| Value::Object(serde_json::Map::new()));

        let response = self
            .client
            .execute(ExecuteRequest {
                slug: slug.to_owned(),
                input_json: args.to_string(),
                idempotency_key: idempotency_key.to_owned(),
                ..Default::default()
            })
            .await
            .map_err(|e| TaskError::Failed(e.to_string()))?
            .into_owned();

        match response.status.as_str() {
            "ok" => serde_json::from_str(&response.output_json).map_err(|e| TaskError::Failed(e.to_string())),
            "requires_approval" => Err(TaskError::Failed("tool requires approval — not yet wired in Engine".to_owned())),
            _ => Err(TaskError::Failed(response.error_message)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::proto::tools::v1::{
        ActivateToolRequest, ActivateToolResponse, CreateToolRequest, CreateToolResponse,
        ExecuteResponse, ListToolsResponse, ToolService, ValidateToolRequest, ValidateToolResponse,
    };
    use connectrpc::{RequestContext, Response, Router as ConnectRouter, ServiceRequest, ServiceResult};
    use std::sync::{Arc, Mutex};

    struct FakeToolService {
        received: Mutex<Vec<ExecuteRequest>>,
        status: String,
        output_json: String,
    }

    #[allow(refining_impl_trait)]
    impl ToolService for FakeToolService {
        async fn execute(&self, _ctx: RequestContext, request: ServiceRequest<'_, ExecuteRequest>) -> ServiceResult<ExecuteResponse> {
            self.received.lock().expect("lock").push(request.to_owned_message());
            Response::ok(ExecuteResponse { status: self.status.clone(), output_json: self.output_json.clone(), ..Default::default() })
        }
        async fn list_tools(&self, _ctx: RequestContext, _request: ServiceRequest<'_, ListToolsRequest>) -> ServiceResult<ListToolsResponse> {
            Response::ok(ListToolsResponse { tools: vec![Tool { slug: "web_search".to_owned(), ..Default::default() }], ..Default::default() })
        }
        async fn create_tool(&self, _ctx: RequestContext, _request: ServiceRequest<'_, CreateToolRequest>) -> ServiceResult<CreateToolResponse> {
            Response::ok(CreateToolResponse::default())
        }
        async fn validate_tool(&self, _ctx: RequestContext, _request: ServiceRequest<'_, ValidateToolRequest>) -> ServiceResult<ValidateToolResponse> {
            Response::ok(ValidateToolResponse::default())
        }
        async fn activate_tool(&self, _ctx: RequestContext, _request: ServiceRequest<'_, ActivateToolRequest>) -> ServiceResult<ActivateToolResponse> {
            Response::ok(ActivateToolResponse::default())
        }
    }

    async fn serve(fake: Arc<FakeToolService>) -> String {
        let connect = ConnectRouter::new().add_service(fake);
        let app = axum::Router::new().fallback_service(connect.into_axum_service());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("addr");
        tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
        format!("http://{address}")
    }

    #[tokio::test]
    async fn execute_reads_the_tool_call_from_state_and_returns_the_output() {
        let fake = Arc::new(FakeToolService { received: Mutex::new(Vec::new()), status: "ok".to_owned(), output_json: r#"{"results": []}"#.to_owned() });
        let url = serve(Arc::clone(&fake)).await;
        let executor = ToolTaskExecutor::new(&url).expect("client");
        let state = serde_json::json!({"llm": {"tool_call": {"name": "web_search", "args": {"query": "rust"}}}});

        let output = executor.execute("tool", &serde_json::json!({}), &state, "e:tool:0").await.expect("execute");

        assert_eq!(output, serde_json::json!({"results": []}));
        let received = fake.received.lock().expect("lock");
        assert_eq!(received[0].slug, "web_search");
        assert_eq!(received[0].input_json, r#"{"query":"rust"}"#);
    }

    #[tokio::test]
    async fn execute_maps_requires_approval_to_a_clear_error() {
        let fake = Arc::new(FakeToolService { received: Mutex::new(Vec::new()), status: "requires_approval".to_owned(), output_json: String::new() });
        let url = serve(fake).await;
        let executor = ToolTaskExecutor::new(&url).expect("client");
        let state = serde_json::json!({"llm": {"tool_call": {"name": "send_email", "args": {}}}});

        let error = executor.execute("tool", &serde_json::json!({}), &state, "e:tool:0").await.unwrap_err();

        assert!(matches!(error, TaskError::Failed(msg) if msg.contains("requires approval")));
    }

    #[tokio::test]
    async fn list_tools_returns_the_catalog() {
        let fake = Arc::new(FakeToolService { received: Mutex::new(Vec::new()), status: "ok".to_owned(), output_json: "{}".to_owned() });
        let url = serve(fake).await;
        let executor = ToolTaskExecutor::new(&url).expect("client");

        let tools = executor.list_tools().await.expect("list");

        assert_eq!(tools[0].slug, "web_search");
    }
}
```

Add `pub mod tool;` to `services/engine/src/executors/mod.rs`.

- [ ] **Step 2: Run**

Run: `cargo test -p engine executors::tool:: 2>&1 | tail -30`
Expected: PASS, 3 tests. No Docker, no Tool Service running — a fake server on loopback.

- [ ] **Step 3: Commit**

```bash
git add services/engine/src/executors/tool.rs services/engine/src/executors/mod.rs
git commit -m "engine: ToolTaskExecutor — Tool Service adapter for kind=\"tool\""
```

## Task 14: `services/engine` — wire `ToolTaskExecutor` into the `Dispatcher`, inject the tool catalog before `llm`

**Files:**
- Modify: `services/engine/src/dispatch.rs`
- Modify: `services/engine/src/executors/llm.rs`
- Modify: `services/engine/src/main.rs`
- Modify: `compose.yaml` (the `engine` service block)

**Interfaces:**
- Consumes: Task 13's `ToolTaskExecutor::{new, list_tools}`.
- Produces: `Dispatcher` now has two `TaskExecutor`s (`llm`, `tool`) and, for any node whose
  `config.tool_calling == true`, calls `tool.list_tools()` and adds the catalog to the `config`
  it passes to `llm` — under `config["available_tools"]`, the key `build_prompt` reads.

- [ ] **Step 1: Write the failing test — `Dispatcher` injects the catalog and routes `"tool"`**

Add to `services/engine/src/dispatch.rs` (test module; the fakes reuse the same in-process-server
pattern as Task 13's tests, one fake per executor kind so the test only depends on Dispatcher's
own routing/injection logic, not on any other module's fakes):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::executors::llm::LlmTaskExecutor;
    use crate::executors::tool::ToolTaskExecutor;
    use common::proto::llm_router::v1::{CompleteRequest, CompleteResponse, LlmRouterService};
    use common::proto::tools::v1::{
        ActivateToolRequest, ActivateToolResponse, CreateToolRequest, CreateToolResponse,
        ExecuteRequest, ExecuteResponse, ListToolsRequest, ListToolsResponse, Tool, ToolService,
        ValidateToolRequest, ValidateToolResponse,
    };
    use connectrpc::{RequestContext, Response, Router as ConnectRouter, ServiceRequest, ServiceResult};
    use std::sync::{Arc, Mutex};

    struct FakeLlm {
        received_config_tools: Mutex<Option<Value>>,
    }

    #[allow(refining_impl_trait)]
    impl LlmRouterService for FakeLlm {
        async fn complete(&self, _ctx: RequestContext, request: ServiceRequest<'_, CompleteRequest>) -> ServiceResult<CompleteResponse> {
            let msg = request.to_owned_message();
            *self.received_config_tools.lock().expect("lock") = Some(json!(msg.system_prompt));
            Response::ok(CompleteResponse { content: r#"{"tool_call": null, "reply": "ok"}"#.to_owned(), ..Default::default() })
        }
    }

    struct FakeTool;

    #[allow(refining_impl_trait)]
    impl ToolService for FakeTool {
        async fn execute(&self, _ctx: RequestContext, _request: ServiceRequest<'_, ExecuteRequest>) -> ServiceResult<ExecuteResponse> {
            Response::ok(ExecuteResponse { status: "ok".to_owned(), output_json: "{}".to_owned(), ..Default::default() })
        }
        async fn list_tools(&self, _ctx: RequestContext, _request: ServiceRequest<'_, ListToolsRequest>) -> ServiceResult<ListToolsResponse> {
            Response::ok(ListToolsResponse { tools: vec![Tool { slug: "web_search".to_owned(), description: "search the web".to_owned(), ..Default::default() }], ..Default::default() })
        }
        async fn create_tool(&self, _ctx: RequestContext, _request: ServiceRequest<'_, CreateToolRequest>) -> ServiceResult<CreateToolResponse> {
            Response::ok(CreateToolResponse::default())
        }
        async fn validate_tool(&self, _ctx: RequestContext, _request: ServiceRequest<'_, ValidateToolRequest>) -> ServiceResult<ValidateToolResponse> {
            Response::ok(ValidateToolResponse::default())
        }
        async fn activate_tool(&self, _ctx: RequestContext, _request: ServiceRequest<'_, ActivateToolRequest>) -> ServiceResult<ActivateToolResponse> {
            Response::ok(ActivateToolResponse::default())
        }
    }

    async fn serve_llm(fake: Arc<FakeLlm>) -> String {
        let connect = ConnectRouter::new().add_service(fake);
        let app = axum::Router::new().fallback_service(connect.into_axum_service());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("addr");
        tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
        format!("http://{address}")
    }

    async fn serve_tool(fake: Arc<FakeTool>) -> String {
        let connect = ConnectRouter::new().add_service(fake);
        let app = axum::Router::new().fallback_service(connect.into_axum_service());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("addr");
        tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
        format!("http://{address}")
    }

    #[tokio::test]
    async fn tool_kind_routes_to_the_tool_executor() {
        let tool_url = serve_tool(Arc::new(FakeTool)).await;
        let llm_url = serve_llm(Arc::new(FakeLlm { received_config_tools: Mutex::new(None) })).await;
        let dispatcher = Dispatcher {
            llm: LlmTaskExecutor::new(&llm_url).expect("llm client"),
            tool: ToolTaskExecutor::new(&tool_url).expect("tool client"),
        };
        let state = json!({"llm": {"tool_call": {"name": "web_search", "args": {}}}});

        let output = dispatcher.execute("tool", &json!({}), &state, "e:t:0").await.expect("execute");

        assert_eq!(output, json!({}));
    }

    #[tokio::test]
    async fn llm_kind_with_tool_calling_gets_the_catalog_injected_into_its_system_prompt() {
        let tool_url = serve_tool(Arc::new(FakeTool)).await;
        let llm = Arc::new(FakeLlm { received_config_tools: Mutex::new(None) });
        let llm_url = serve_llm(Arc::clone(&llm)).await;
        let dispatcher = Dispatcher {
            llm: LlmTaskExecutor::new(&llm_url).expect("llm client"),
            tool: ToolTaskExecutor::new(&tool_url).expect("tool client"),
        };
        let config = json!({"tool_calling": true});

        dispatcher.execute("llm", &config, &json!({}), "e:l:0").await.expect("execute");

        let received = llm.received_config_tools.lock().expect("lock").clone().expect("called");
        assert!(received.as_str().unwrap().contains("web_search"));
        assert!(received.as_str().unwrap().contains("search the web"));
    }

    #[tokio::test]
    async fn llm_kind_without_tool_calling_never_calls_list_tools() {
        // A tool_url pointing nowhere real: if the Dispatcher called list_tools for a
        // non-tool-calling node, this would fail with a connection error instead of the
        // expected plain reply.
        let llm = Arc::new(FakeLlm { received_config_tools: Mutex::new(None) });
        let llm_url = serve_llm(Arc::clone(&llm)).await;
        let dispatcher = Dispatcher {
            llm: LlmTaskExecutor::new(&llm_url).expect("llm client"),
            tool: ToolTaskExecutor::new("http://127.0.0.1:1").expect("tool client"),
        };

        let output = dispatcher.execute("llm", &json!({}), &json!({}), "e:l:0").await.expect("execute");

        assert_eq!(output["reply"], json!("ok"));
    }
}
```

- [ ] **Step 2: Run to see it fail**

Run: `cargo test -p engine dispatch:: 2>&1 | tail -30`
Expected: FAIL to compile — `Dispatcher` has no `tool` field yet, and `dispatch::tests` is new.

- [ ] **Step 3: Implement — `Dispatcher` gains a `tool` executor and injects the catalog**

Replace `services/engine/src/dispatch.rs`'s non-test content with:

```rust
// The one place Task.kind strings map to a real TaskExecutor implementation. For "llm" nodes
// whose config asks for tool_calling, the Dispatcher fetches the tool catalog from Tool Service
// and folds it into the config it hands to LlmTaskExecutor — LlmTaskExecutor itself has no
// knowledge of Tool Service, it only reads whatever ends up in config["available_tools"].

use engine_core::{TaskError, TaskExecutor};
use serde_json::{Value, json};

use crate::executors::llm::LlmTaskExecutor;
use crate::executors::tool::ToolTaskExecutor;

pub struct Dispatcher {
    pub llm: LlmTaskExecutor,
    pub tool: ToolTaskExecutor,
}

impl TaskExecutor for Dispatcher {
    async fn execute(
        &self,
        kind: &str,
        config: &Value,
        state: &Value,
        idempotency_key: &str,
    ) -> Result<Value, TaskError> {
        match kind {
            "tool" => self.tool.execute(kind, config, state, idempotency_key).await,
            "llm" if config.get("tool_calling").and_then(Value::as_bool) == Some(true) => {
                let tools = self
                    .tool
                    .list_tools()
                    .await
                    .map_err(TaskError::Failed)?
                    .into_iter()
                    .map(|t| json!({"name": t.slug, "description": t.description}))
                    .collect::<Vec<_>>();
                let mut config = config.clone();
                if let Some(object) = config.as_object_mut() {
                    object.insert("available_tools".to_owned(), Value::Array(tools));
                }
                self.llm.execute(kind, &config, state, idempotency_key).await
            }
            "llm" => self.llm.execute(kind, config, state, idempotency_key).await,
            other => Err(TaskError::Failed(format!(
                "no TaskExecutor wired for kind {other:?}"
            ))),
        }
    }
}
```

- [ ] **Step 4: Have `build_prompt` render `available_tools` into the system prompt**

`LlmTaskExecutor` doesn't know Tool Service exists — it only reads a JSON array the Dispatcher
already placed under `config["available_tools"]`, same as it already reads `tool_calling` and
`system_prompt`. In `services/engine/src/executors/llm.rs`, change `build_prompt`:

```rust
#[must_use]
pub fn build_prompt(config: &Value, state: &Value) -> (String, String) {
    let mut system = config
        .get("system_prompt")
        .and_then(Value::as_str)
        .unwrap_or(DEFAULT_SYSTEM_PROMPT)
        .to_owned();
    if config.get("tool_calling").and_then(Value::as_bool) == Some(true) {
        system.push_str(TOOL_CALLING_INSTRUCTIONS);
    }
    if let Some(tools) = config.get("available_tools").and_then(Value::as_array) {
        system.push_str("\n\nAvailable tools:\n");
        for tool in tools {
            let name = tool.get("name").and_then(Value::as_str).unwrap_or_default();
            let description = tool.get("description").and_then(Value::as_str).unwrap_or_default();
            system.push_str(&format!("- {name}: {description}\n"));
        }
    }
    // ... rest of the function is unchanged (question/tool_result handling)
```

Add one more unit test to `executors/llm.rs`'s existing `#[cfg(test)] mod tests`:

```rust
    #[test]
    fn build_prompt_renders_available_tools_into_the_system_prompt() {
        let config = json!({"available_tools": [{"name": "web_search", "description": "search the web"}]});
        let (system, _) = build_prompt(&config, &json!({}));
        assert!(system.contains("web_search"));
        assert!(system.contains("search the web"));
    }
```

- [ ] **Step 5: Run tests to see them pass**

Run: `cargo test -p engine 2>&1 | tail -40`
Expected: PASS — `dispatch::tests` (3), `executors::llm::tests` (now 6), everything else in
`services/engine` unaffected.

- [ ] **Step 6: Wire `TOOL_SERVICE_URL` into `main.rs`, `compose.yaml`, `.env.example`**

In `services/engine/src/main.rs`, add `use engine::executors::tool::ToolTaskExecutor;` next to
the existing `use engine::executors::llm::LlmTaskExecutor;`, and change the `Dispatcher`
construction inside `async fn main`:

```rust
    let executor = dispatch::Dispatcher {
        llm: LlmTaskExecutor::new(&env("LLM_ROUTER_URL")?)?,
        tool: ToolTaskExecutor::new(&env("TOOL_SERVICE_URL")?)?,
    };
```

In `compose.yaml`'s `engine` service block, add the env var and the dependency (`tool` must be
healthy before `engine` starts, same as `llm-router` already is — an execution that hits a `tool`
node before Tool Service is reachable would otherwise fail every graph run until it comes up):

```yaml
  engine:
    build:
      dockerfile: services/engine/Dockerfile
    develop: *rebuild-on-save
    environment:
      LLM_ROUTER_URL: http://llm-router:8083
      TOOL_SERVICE_URL: http://tool:8087
      ENGINE_TERMINAL_RETENTION:
      ENGINE_EVENTS_RETENTION_MONTHS:
      DATABASE_URL: postgres://engine_user:${ENGINE_DB_PASSWORD:?set it in .env, see .env.example}@postgres:5432/${POSTGRES_DB:-app}
    depends_on:
      postgres:
        condition: service_healthy
      llm-router:
        condition: service_healthy
      tool:
        condition: service_healthy
```

- [ ] **Step 7: Rebuild and restart `engine` live, confirm it comes up healthy with the new dependency**

```bash
export PATH="$HOME/.cargo/bin:$PATH"
docker compose -p aiengineeringboilerplate build engine
docker compose -p aiengineeringboilerplate up -d engine
docker compose -p aiengineeringboilerplate ps engine tool
```
Expected: both `engine` and `tool` show `healthy`. If `engine` restarts in a crash loop, check
`docker compose -p aiengineeringboilerplate logs engine --tail 50` — the most likely cause is
`TOOL_SERVICE_URL` missing from the container's environment (an `.env` not updated, or compose
not reloaded with `--build`).

- [ ] **Step 8: Commit**

```bash
git add services/engine/src/dispatch.rs services/engine/src/executors/llm.rs \
        services/engine/src/main.rs compose.yaml
git commit -m "engine: wire ToolTaskExecutor into Dispatcher, inject tool catalog before llm calls"
```

## Task 15: Final verification — workspace-wide checks, Docker, live end-to-end smoke test

**Files:** none created; this task only runs commands.

**Interfaces:** none — this is the plan's closing gate, exercising everything Tasks 1-14 built
together through the real running stack.

- [ ] **Step 1: Workspace-wide format, lint, test**

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -60
cargo test --workspace 2>&1 | tail -80
```
Expected: clippy clean, every crate's tests pass — `common`, `engine-core`, `services/engine`,
`services/tool`, `services/crawler` (Task 2 moved `extract` out of it) all included.

- [ ] **Step 2: Full stack up**

```bash
docker compose -p aiengineeringboilerplate build tool engine
docker compose -p aiengineeringboilerplate up -d
docker compose -p aiengineeringboilerplate ps
```
Expected: every service `healthy`, including `tool` and `engine`. Re-check with
`docker compose -p aiengineeringboilerplate logs tool --tail 50` if `tool` doesn't reach healthy
— the most common cause at this point is `tool_user`/`tool` schema not yet bootstrapped on the
live Postgres volume (Task 12 Step 5 covers this and must have been run once already).

- [ ] **Step 3: Confirm the 4 system tools seeded themselves on `tool`'s startup**

```bash
docker compose -p aiengineeringboilerplate exec postgres \
  psql -U postgres -d app -c \
  "SELECT slug, status, risk FROM tool.tools WHERE user_id IS NULL ORDER BY slug;"
```
Expected: 4 rows — `kb_read_document`, `kb_search`, `web_fetch`, `web_search` — all `status =
active` (2, `ToolStatus::Active`'s `i16`), `kb_search`/`kb_read_document`/`web_fetch` at `risk =
0` (`ReadOnly`).

- [ ] **Step 4: Live end-to-end smoke test — a real `agent_graph` execution that calls `kb_search`
  through the real running `knowledge-base` service**

This is the test the whole plan has been building toward: Engine's `agent_graph()` calling into
Tool Service's `kb_search`, which calls the real `knowledge-base` service, which has real crawled
`academy.claude.com` pages from earlier in this project. No fake server anywhere in this step —
every hop is the actual Docker container.

```bash
docker compose -p aiengineeringboilerplate exec engine sh -c '
  wget -q -O- --header="Content-Type: application/json" --post-data="{
    \"graph_id\": \"agent\",
    \"input_json\": \"{\\\"question\\\": \\\"What is Claude Code? Use the kb_search tool to find out.\\\"}\"
  }" http://localhost:8085/engine.v1.EngineService/StartExecution
'
```
(`agent_graph` is already registered under graph_id `"agent"` by `register_builtin_graphs` at
Engine startup — see `services/engine/src/main.rs`; no `RegisterGraph` call needed.)

Expected: a JSON response with a non-empty `execution_id`. Poll it to completion:

```bash
EXECUTION_ID=<the id from the previous response>
docker compose -p aiengineeringboilerplate exec engine sh -c "
  wget -q -O- --header='Content-Type: application/json' --post-data='{\"execution_id\": \"$EXECUTION_ID\"}' \
    http://localhost:8085/engine.v1.EngineService/GetExecution
"
```
Expected, once `status` reaches `completed` (poll every couple of seconds, a handful of times —
the graph makes 2 real LLM calls plus a real Tool Service round trip): the execution's final
`state_json` contains a non-empty `reply` string that references Claude Code, and the
`execution_events` history (visible via `StreamEvents`, or by inspecting `engine.execution_events`
directly) shows a `tool` node whose recorded output is the `kb_search` result, not an error.

If the model doesn't call `kb_search` on the first attempt (a Medium-tier model isn't guaranteed
to follow the JSON tool-call contract every time), that's a finding about `agent_graph`'s prompt
tuning, not about Tool Service — Tool Service's own job (Tasks 1-14) is done once a `tool` node,
when reached, executes correctly end-to-end. Re-run with a more leading question
(`"Use the kb_search tool to look up: what is Claude Code?"`) if needed to force the branch.

- [ ] **Step 5: Confirm `web_search`'s absence of a real key fails cleanly, not with a panic**

`web_search` has no real `BRAVE_SEARCH_API_KEY` in this environment (Task 6's design note) — this
step only confirms the failure mode is the ordinary `ExecuteResponse{status: "error", ...}` path,
not a crash that takes `tool` down for every other tool:

```bash
docker compose -p aiengineeringboilerplate exec tool sh -c '
  wget -q -O- --header="Content-Type: application/json" --post-data="{
    \"slug\": \"web_search\", \"input_json\": \"{\\\"query\\\": \\\"rust\\\"}\"
  }" http://localhost:8087/tools.v1.ToolService/Execute
'
docker compose -p aiengineeringboilerplate ps tool
```
Expected: the `Execute` call returns `{"status": "error", ...}` (Brave rejects the placeholder
key with 401/403, mapped by `BraveSearchProvider` to `Err`, mapped by `run_system_tool` to
`ExecuteOutcome::Error`), and `tool` is still `healthy` afterward — one failed tool call must
never take the whole service down.

- [ ] **Step 6: Run the code-review skill's gates**

Per the user's standing instruction, gates run once at the end of the day's work, not per task —
this is that point for the Tool Service unit of work. Invoke the `code-review` skill against the
full diff since `main`'s pre-Tool-Service state (Task 1's starting commit) and address any
findings before considering Tool Service done.

- [ ] **Step 7: Final commit (if Step 6 produced fixes)**

```bash
git add -A
git commit -m "tool: address code-review gate findings"
```

If Step 6 found nothing, there is no Step 7 commit — Task 14's commit is the last one.
