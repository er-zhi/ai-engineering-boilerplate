# Chat Service Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build `services/chat` — the session/topic-tree orchestration layer described in the
spec — wired to the already-running `engine` (via `StartExecution`/`Interrupt`/`StreamEvents`)
and exposed through `gateway`, with a minimal working chat frontend proving the spec's own
end-to-end scenario: two topics, each running `kb_search`/`web_search` through the now-complete
Tool Service, with a notification when a background topic completes.

**Architecture:** A stateless Connect RPC service (`CreateTopic`/`SetFocus`/`SendTurn`/
`GetSession`/`StreamEvents`) backed by three reference tables (`chat.sessions`, `chat.topics`,
`chat.messages`) — no new append-only log table of its own. `chat.topics.status` is the durable
source of truth for a topic's lifecycle (restart-recovery reads it directly); the *execution
detail* of a topic (every LLM/tool step) stays exactly where it already lives and is already
partitioned — Engine's own `execution_events`, reached by proxying Engine's `StreamEvents`
internally. Chat's own `StreamEvents` RPC serves session-level topic-lifecycle events (created,
started, progress, completed, notification) over a live broadcast channel, seeded from
`chat.topics`' current rows on connect.

**Tech Stack:** Rust 2024, SeaORM (entity-first schema-sync), Connect RPC (`connectrpc` +
`buffa`), Postgres, testcontainers. No new external crate beyond `tokio::sync::broadcast`
(already in `tokio`, already a workspace dependency).

**Spec:** `docs/superpowers/specs/2026-09-15-session-topics-design.md` — read it before
starting. Already approved (committed as part of Engine's own squashed work, `b7eb6de`); this
plan does not re-litigate the model, only implements it. `.temp/plans/todo.md`'s "## 4. Chat
Service + фронт" section is the concrete checklist this plan turns into tasks; where the two
disagree, the spec wins (the spec is newer and explicitly supersedes `.temp/plans/2026-09-12.md`'s
older Chat notes).

**Scope note (read before objecting to anything look missing):** the spec's own "Вне скоупа"
section explicitly excludes **visual design of the topic panel** and **automatic LLM
classification of which topic a reply belongs to** — both appear as aspirational bullets in
`todo.md`'s checklist (the topic-tree SVG, the execution-graph SVG, multi-topic-from-one-message
detection) but are not commitments the approved spec makes. This plan builds exactly what the
spec commits to: a working transcript, a topic panel with status, notifications, and explicit
topic creation via a UI action — no SVG visualizations, no LLM-based topic classification. Both
are legitimate future work, not gaps in this plan.

## Global Constraints

- Strict lints: `unsafe_code = "forbid"`, clippy `all = deny`, `unwrap_used = "deny"`,
  `too_many_lines = "deny"`, `cognitive_complexity = "deny"` — every new file.
- No `async_trait`: ports use `impl Future<Output = ...> + Send` in return position.
- Entity-first SeaORM: `db.get_schema_registry("chat::entity::*").sync(&db)`, no hand-written
  `.sql` migrations.
- Every JSON column is `jsonb` (`column_type = "JsonBinary"`) — no exceptions
  (`.agents/skills/code-review/gate-database/SKILL.md`).
- No new append-only/log-shaped table: `chat.sessions`/`chat.topics`/`chat.messages` are all
  reference data (grow with entities, not with time) per `gate-database`'s "Growth" rules —
  bounded by real usage, never partitioned. Execution-level detail (every LLM/tool step) stays
  in Engine's own already-partitioned `execution_events`; Chat never duplicates it.
- `chat.messages` uniqueness on `(topic_id, turn_id)` — both columns are `NOT NULL`, so this is
  a plain SeaORM `unique_key`, no manual-index escape hatch needed (unlike Tool Service's
  nullable-`user_id` case).
- `chat.sessions` uniqueness on `user_id` — one session per user for this scope (see Task 3's
  design note); also a plain `unique_key`.
- Work directly on `main`, no worktree/branch — same as every other unit of work tonight.
- Docker: `docker compose -p aiengineeringboilerplate` — the real running stack (7 services:
  postgres/crawler/gateway/knowledge-base/llm-router/engine/tool) — never
  `docker compose down -v`, never touch data outside the `chat` schema.
- Docker health check: `timeout 10 docker ps` before any build/up command in every task. If it
  hangs, stop and report BLOCKED — do not retry, do not attempt to fix Docker.
- `export PATH="$HOME/.cargo/bin:$PATH"` before any `cargo` command in every task.
- Commit trailers after a blank line: `Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>` /
  `Claude-Session: https://claude.ai/code/session_014WYAbXrDaF8pTeCytR8Uqg` — written completely
  literally regardless of which model executes a task. Stage only the files a task lists; never
  `git add -A`.
- This repo has a custom `rustfmt.toml` with a narrower max line width than this plan's code
  blocks may show wrapped at. Type the content, then run `cargo fmt` and trust its output —
  semantic content must match exactly, line-wrap positions don't need to.

---

## Task 1: `chat` crate scaffold and entities

**Files:**
- Create: `services/chat/Cargo.toml`
- Create: `services/chat/src/lib.rs`
- Create: `services/chat/src/main.rs`
- Create: `services/chat/src/entity/mod.rs`
- Create: `services/chat/src/entity/session.rs`
- Create: `services/chat/src/entity/topic.rs`
- Create: `services/chat/src/entity/message.rs`
- Modify: root `Cargo.toml` (add `"services/chat"` to `[workspace] members`)

**Interfaces:**
- Produces: `entity::session::{Model, Entity, ActiveModel}`, `entity::topic::{Model, Entity,
  ActiveModel, Status}`, `entity::message::{Model, Entity, ActiveModel}` — consumed from Task 3
  onward.

- [ ] **Step 1: Workspace wiring and `Cargo.toml`**

Add `"services/chat"` to root `Cargo.toml`'s `[workspace] members`, alphabetically after
`"services/tool"`.

`services/chat/Cargo.toml`:
```toml
[package]
name = "chat"
edition.workspace = true
rust-version.workspace = true
version.workspace = true
publish.workspace = true

[lib]
name = "chat"
path = "src/lib.rs"

[lints]
workspace = true

[dependencies]
axum = { workspace = true }
buffa = { workspace = true }
chrono = { workspace = true }
common = { workspace = true }
connectrpc = { workspace = true, features = ["axum", "client"] }
futures = { workspace = true }
sea-orm = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
thiserror = { workspace = true }
tokio = { workspace = true, features = ["macros", "rt-multi-thread", "sync"] }
tokio-stream = { workspace = true, features = ["sync"] }
tracing = { workspace = true }
uuid = { workspace = true }

[features]
test-support = ["common/test-support"]

[dev-dependencies]
common = { workspace = true, features = ["test-support"] }
tokio = { workspace = true, features = ["test-util"] }
```

- [ ] **Step 2: The entities**

`services/chat/src/entity/session.rs`:
```rust
// chat.sessions: one row per user — reference data, grows with users, not time, no
// partitioning needed (gate-database "Growth"). One session per user for this scope (see this
// plan's Task 3 design note); focus_topic_id is a pointer, not a topic's own property (spec:
// "Фокус — указатель, не свойство темы"), so it lives here.

use sea_orm::entity::prelude::*;

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "sessions", schema_name = "chat")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    #[sea_orm(unique)]
    pub user_id: Uuid,
    pub focus_topic_id: Option<i64>,
    pub created_at: DateTimeUtc,
}

impl ActiveModelBehavior for ActiveModel {}
```

`services/chat/src/entity/topic.rs`:
```rust
// chat.topics: the tree of topics per session — reference data, grows with topic count, not
// time, no partitioning needed. `status` is the durable source of truth for restart recovery
// (spec: "темы восстанавливаются из лога" — here, from this row directly: Chat keeps no
// append-only event log of its own, Engine's already-partitioned execution_events is the log
// of record for a topic's execution detail).

use sea_orm::entity::prelude::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "i16", db_type = "SmallInteger")]
pub enum Status {
    #[sea_orm(num_value = 0)]
    Queued,
    #[sea_orm(num_value = 1)]
    Running,
    #[sea_orm(num_value = 2)]
    Completed,
    #[sea_orm(num_value = 3)]
    Failed,
    #[sea_orm(num_value = 4)]
    Cancelled,
}

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "topics", schema_name = "chat")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub session_id: Uuid,
    pub parent_id: Option<i64>,
    #[sea_orm(column_type = "String(StringLen::N(128))")]
    pub title: String,
    pub status: Status,
    /// Unset only while `Queued` — a queued topic has no Engine execution yet.
    pub execution_id: Option<Uuid>,
    pub result_summary: Option<String>,
    #[sea_orm(column_type = "JsonBinary")]
    pub artifact_ids: Json,
    pub created_at: DateTimeUtc,
    pub updated_at: DateTimeUtc,
}

impl ActiveModelBehavior for ActiveModel {}
```

`services/chat/src/entity/message.rs`:
```rust
// chat.messages: user-submitted turns only — not the LLM/tool blow-by-blow, which stays in
// Engine's own execution_events. Enough to dedup by turn_id (spec: "дедуп реплик по turn_id",
// enforced here by a plain unique constraint — no separate idempotency abstraction needed,
// both columns are NOT NULL) and reconstruct which turns went where.

use sea_orm::entity::prelude::*;

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "messages", schema_name = "chat")]
#[sea_orm(unique_key = "topic_turn", columns = ["topic_id", "turn_id"])]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub topic_id: i64,
    pub turn_id: Uuid,
    #[sea_orm(column_type = "Text")]
    pub content: String,
    pub created_at: DateTimeUtc,
}

impl ActiveModelBehavior for ActiveModel {}
```

`services/chat/src/entity/mod.rs`:
```rust
pub mod message;
pub mod session;
pub mod topic;
```

- [ ] **Step 3: `lib.rs` and a schema-sync-only `main.rs`**

`services/chat/src/lib.rs`:
```rust
// chat as a library: everything main.rs assembles, exposed for unit- and integration-testing
// directly (main.rs stays a thin binary entry point). Each later task adds its own `pub mod`.

pub mod entity;
```

`services/chat/src/main.rs`:
```rust
// chat: session/topic orchestration over Engine. Connects to Postgres, syncs its schema. This
// early version only proves the crate and its entities compile and the schema comes up.

use sea_orm::Database;

fn env(name: &str) -> Result<String, String> {
    std::env::var(name).map_err(|_| format!("{name} is not set"))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    common::logging::init();

    let database_url = env("DATABASE_URL")?;
    let db = Database::connect(&database_url).await?;
    db.get_schema_registry("chat::entity::*").sync(&db).await?;

    tracing::info!("chat: schema synced, no RPC surface yet");
    Ok(())
}
```

- [ ] **Step 4: Verify**

Run: `export PATH="$HOME/.cargo/bin:$PATH" && cargo check -p chat --all-features 2>&1 | tail -30`
Expected: compiles clean.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock services/chat/
git commit -m "chat: crate scaffold, entities (sessions/topics/messages)"
```

---

## Task 2: `common/proto/chat/v1/chat.proto`

**Files:**
- Create: `common/proto/chat/v1/chat.proto`
- Modify: `common/build.rs`

**Interfaces:**
- Produces: `common::proto::chat::v1::{ChatService, ChatServiceClient, CreateTopicRequest,
  CreateTopicResponse, SetFocusRequest, SetFocusResponse, SendTurnRequest, SendTurnResponse,
  GetSessionRequest, GetSessionResponse, Topic, StreamEventsRequest, ChatEvent}` — generated,
  consumed from Task 5 onward.

- [ ] **Step 1: Write the proto file**

`common/proto/chat/v1/chat.proto`:
```proto
syntax = "proto3";

package chat.v1;

service ChatService {
  rpc CreateTopic(CreateTopicRequest) returns (CreateTopicResponse);
  rpc SetFocus(SetFocusRequest) returns (SetFocusResponse);
  rpc SendTurn(SendTurnRequest) returns (SendTurnResponse);
  rpc GetSession(GetSessionRequest) returns (GetSessionResponse);
  rpc StreamEvents(StreamEventsRequest) returns (stream ChatEvent);
}

message CreateTopicRequest {
  optional string parent_id = 1;  // unset = new root topic
  string title = 2;
  string input_json = 3;          // merged into the new topic's execution's initial state
}
message CreateTopicResponse {
  string topic_id = 1;
  string status = 2;              // "running" | "queued" — see policy in Task 3
}

message SetFocusRequest {
  string topic_id = 1;
}
message SetFocusResponse {}

message SendTurnRequest {
  string turn_id = 1;   // idempotency key: a repeat with the same turn_id is a no-op
  string content = 2;   // routed to the session's current focus topic
}
message SendTurnResponse {
  string topic_id = 1;  // the topic that actually received it (the focus at send time)
}

message GetSessionRequest {}
message GetSessionResponse {
  optional string focus_topic_id = 1;
  repeated Topic topics = 2;
}
message Topic {
  string id = 1;
  optional string parent_id = 2;
  string title = 3;
  string status = 4;              // "queued"|"running"|"completed"|"failed"|"cancelled"
  optional string execution_id = 5;
  optional string result_summary = 6;
}

message StreamEventsRequest {}
message ChatEvent {
  string topic_id = 1;
  // "topic_created"|"topic_queued"|"topic_started"|"focus_changed"|"topic_progress"|
  // "topic_completed"|"topic_failed"|"topic_cancelled"|"notification"
  string kind = 2;
  string payload_json = 3;
  string occurred_at = 4;         // RFC 3339
}
```

- [ ] **Step 2: Wire codegen**

`common/build.rs` — add `"proto/chat/v1/chat.proto"` to the `.files(&[...])` list, alphabetically
(after `"proto/chat"` sorts before `"proto/crawler"`, so it becomes the new first entry — check
the existing list's actual order and place it correctly, matching whatever convention the
existing 5 entries already use).

- [ ] **Step 3: Verify**

Run: `cargo check -p common 2>&1 | tail -30`
Expected: compiles clean, `common::proto::chat::v1::*` now exists.

- [ ] **Step 4: Commit**

```bash
git add common/proto/chat/v1/chat.proto common/build.rs
git commit -m "common: add chat/v1 proto contract"
```

---

## Task 3: `error.rs` + `session_manager.rs` — session lookup, `GetSession`

**Files:**
- Create: `services/chat/src/error.rs`
- Create: `services/chat/src/session_manager.rs`
- Modify: `services/chat/src/lib.rs`

**Interfaces:**
- Consumes: `entity::{session, topic}` (Task 1).
- Produces: `ChatError` (thiserror enum, `From<ChatError> for ConnectError`); `SessionManager {
  db: DatabaseConnection }` with `new(db) -> Self`, `async fn get_or_create_session(&self,
  user_id: Uuid) -> Result<entity::session::Model, ChatError>`, `async fn get_session_view(&self,
  user_id: Uuid) -> Result<(Option<i64>, Vec<entity::topic::Model>), ChatError>` — Task 5 adds
  `TopicManager`, which wraps a `SessionManager`.

**Design note — one session per user_id:** the spec doesn't detail session creation/multiplicity;
the opening line frames it as "Один чат" (one chat) per user. This plan keeps that reading:
`get_or_create_session` is idempotent (insert-or-fetch on the `user_id` unique constraint), no
separate `CreateSession` RPC. If multiple concurrent chats per user are ever wanted, that's a
new, separate requirement — not a gap here.

- [ ] **Step 1: `error.rs`**

```rust
// One error enum for the service, one place that maps it to Connect codes — same shape as
// services/tool/src/error.rs.

use connectrpc::ConnectError;

#[derive(Debug, thiserror::Error)]
pub enum ChatError {
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("topic not found: {0}")]
    TopicNotFound(i64),
    #[error("no focus topic set")]
    NoFocus,
    #[error("database error: {0}")]
    Db(#[from] sea_orm::DbErr),
    #[error("engine call failed: {0}")]
    Engine(String),
}

impl From<ChatError> for ConnectError {
    fn from(error: ChatError) -> Self {
        match &error {
            ChatError::InvalidRequest(_) => ConnectError::invalid_argument(error.to_string()),
            ChatError::TopicNotFound(_) => ConnectError::not_found(error.to_string()),
            ChatError::NoFocus => ConnectError::failed_precondition(error.to_string()),
            ChatError::Db(_) | ChatError::Engine(_) => ConnectError::internal(error.to_string()),
        }
    }
}
```

Add `pub mod error;` to `services/chat/src/lib.rs`.

- [ ] **Step 2: Write the failing tests**

`services/chat/src/session_manager.rs`:
```rust
// Session lookup/creation — the thin read side TopicManager (Task 5) builds on. One session
// per user_id (see this plan's Task 3 design note): get_or_create_session is idempotent.

use chrono::Utc;
use sea_orm::{ActiveValue::Set, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};
use uuid::Uuid;

use crate::entity::{session, topic};
use crate::error::ChatError;

pub struct SessionManager {
    pub(crate) db: DatabaseConnection,
}

impl SessionManager {
    #[must_use]
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    pub async fn get_or_create_session(&self, user_id: Uuid) -> Result<session::Model, ChatError> {
        if let Some(existing) = session::Entity::find()
            .filter(session::Column::UserId.eq(user_id))
            .one(&self.db)
            .await?
        {
            return Ok(existing);
        }
        let row = session::ActiveModel {
            id: Set(Uuid::new_v4()),
            user_id: Set(user_id),
            focus_topic_id: Set(None),
            created_at: Set(Utc::now()),
        }
        .insert(&self.db)
        .await?;
        Ok(row)
    }

    /// `(focus_topic_id, every topic in the session)` — backs the `GetSession` RPC.
    pub async fn get_session_view(
        &self,
        user_id: Uuid,
    ) -> Result<(Option<i64>, Vec<topic::Model>), ChatError> {
        let session = self.get_or_create_session(user_id).await?;
        let topics = topic::Entity::find()
            .filter(topic::Column::SessionId.eq(session.id))
            .all(&self.db)
            .await?;
        Ok((session.focus_topic_id, topics))
    }
}

#[cfg(all(test, feature = "test-support"))]
mod tests {
    use super::*;

    async fn manager_with() -> (crate::test_db::TestDb, SessionManager) {
        let test = crate::test_db::start().await;
        let manager = SessionManager::new(test.db.clone());
        (test, manager)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_or_create_session_is_idempotent() {
        let (_test, manager) = manager_with().await;
        let user_id = Uuid::new_v4();

        let first = manager.get_or_create_session(user_id).await.expect("first");
        let second = manager
            .get_or_create_session(user_id)
            .await
            .expect("second");

        assert_eq!(first.id, second.id);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_session_view_starts_empty_with_no_focus() {
        let (_test, manager) = manager_with().await;
        let user_id = Uuid::new_v4();

        let (focus, topics) = manager.get_session_view(user_id).await.expect("view");

        assert_eq!(focus, None);
        assert!(topics.is_empty());
    }
}
```

Add `pub mod session_manager;` to `services/chat/src/lib.rs`. Add `services/chat/src/test_db.rs`
(feature-gated `test-support`, `ServiceSchema { schema: "chat", role: "chat_user", password_var:
"CHAT_DB_PASSWORD", entity_prefix: "chat::entity::*" }`, no post-sync index statements needed —
copy `services/tool/src/test_db.rs`'s shape minus the `INDEX_STATEMENTS_CREATED_AFTER_SCHEMA_SYNC`
loop, since Chat has no manual-index escape hatch) and `#[cfg(feature = "test-support")] pub mod
test_db;` to `lib.rs`.

- [ ] **Step 3: Run**

Run: `cargo test -p chat --features test-support session_manager:: 2>&1 | tail -30`
Requires Docker (testcontainers). Expected: PASS, 2 tests.

- [ ] **Step 4: Commit**

```bash
git add services/chat/src/error.rs services/chat/src/session_manager.rs \
        services/chat/src/test_db.rs services/chat/src/lib.rs
git commit -m "chat: error.rs + SessionManager — session lookup, GetSession's read side"
```

---

## Task 4: `events.rs` — the live `ChatEvent` broadcast

**Files:**
- Create: `services/chat/src/events.rs`
- Modify: `services/chat/src/lib.rs`

**Interfaces:**
- Consumes: `common::proto::chat::v1::ChatEvent` (Task 2).
- Produces: `EventBus { sender: tokio::sync::broadcast::Sender<ChatEvent> }` with `new(capacity:
  usize) -> Self`, `fn publish(&self, event: ChatEvent)`, `fn subscribe(&self) ->
  broadcast::Receiver<ChatEvent>` — Task 5 (`TopicManager`) publishes through this; Task 7's
  `StreamEvents` RPC handler subscribes to it.

A `tokio::sync::broadcast` channel is the whole mechanism: no persistence, no cross-instance
fan-out (Chat runs as one instance tonight, matching every other service in this stack). A late
subscriber gets a `chat.topics`-derived snapshot first (Task 7 handles that), then live events
from this bus — a slow subscriber that falls behind the channel's capacity gets `Lagged` and
should re-fetch a fresh snapshot, which Task 7's handler does.

- [ ] **Step 1: Write the failing test**

```rust
// The live topic-lifecycle event bus GetSession's snapshot (Task 3) doesn't cover — a plain
// broadcast channel, one process, no persistence. TopicManager (Task 5) publishes; StreamEvents
// (Task 7) subscribes.

use common::proto::chat::v1::ChatEvent;
use tokio::sync::broadcast;

const DEFAULT_CAPACITY: usize = 256;

pub struct EventBus {
    sender: broadcast::Sender<ChatEvent>,
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new(DEFAULT_CAPACITY)
    }
}

impl EventBus {
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let (sender, _receiver) = broadcast::channel(capacity);
        Self { sender }
    }

    /// No subscribers is not an error — publishing when nobody's listening is normal (e.g. no
    /// chat frontend is currently connected).
    pub fn publish(&self, event: ChatEvent) {
        let _ = self.sender.send(event);
    }

    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<ChatEvent> {
        self.sender.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_subscriber_receives_a_published_event() {
        let bus = EventBus::new(8);
        let mut receiver = bus.subscribe();
        let event = ChatEvent {
            topic_id: "1".to_owned(),
            kind: "topic_progress".to_owned(),
            ..Default::default()
        };

        bus.publish(event.clone());

        let received = receiver.recv().await.expect("recv");
        assert_eq!(received, event);
    }

    #[test]
    fn publishing_with_no_subscribers_does_not_panic() {
        let bus = EventBus::new(8);
        bus.publish(ChatEvent::default());
    }
}
```

Add `pub mod events;` to `services/chat/src/lib.rs`.

- [ ] **Step 2: Run**

Run: `cargo test -p chat events:: 2>&1 | tail -20`
Expected: PASS, 2 tests. No Docker needed.

- [ ] **Step 3: Commit**

```bash
git add services/chat/src/events.rs services/chat/src/lib.rs
git commit -m "chat: EventBus — the live ChatEvent broadcast"
```

---

## Task 5: `engine_client.rs` — the Engine adapter

**Files:**
- Create: `services/chat/src/engine_client.rs`
- Modify: `services/chat/src/lib.rs`

**Interfaces:**
- Consumes: `common::proto::engine::v1::{EngineServiceClient, StartExecutionRequest,
  InterruptRequest, ExecutionEvent, StreamEventsRequest}` (already exists, confirmed this plan's
  own research pass — no proto change needed).
- Produces: `EngineClient { inner: EngineServiceClient<HttpClient> }` with `new(url: &str) ->
  Result<Self, String>`, `async fn start_execution(&self, graph_id: &str, input_json: &str) ->
  Result<Uuid, String>`, `async fn interrupt(&self, execution_id: Uuid, input_json: &str) ->
  Result<(), String>`, `async fn stream_events(&self, execution_id: Uuid) ->
  Result<tokio::sync::mpsc::UnboundedReceiver<ExecutionEvent>, String>` — Task 6
  (`TopicManager`) is the only consumer.

- [ ] **Step 1: Write the failing tests — fake `EngineService`, same in-process pattern as
  every other client this plan/Tool Service built**

**Verified against the real generated client** (`target/debug/build/common-*/out/
engine.v1.engine.__connect.rs`, read directly rather than guessed): a streaming RPC's client
method returns `Result<ServerStream<B, ExecutionEventView<'static>>, ConnectError>` —
`ServerStream` is **not** a `futures::Stream`; it's consumed one message at a time via
`stream.message::<ExecutionEvent>().await` (`Ok(Some(item))` per message, `item.to_owned_message()`
to convert, `Ok(None)` on a clean end, `Err` on any failure — same `.to_owned_message()` pattern
already used for every unary call in this codebase). To avoid spelling out `ServerStream`'s
generic parameters at every call site, `stream_events` below drains it in a spawned task and
hands the caller a plain `tokio::sync::mpsc::UnboundedReceiver<ExecutionEvent>` instead — Task 7's
`watch_topic` just does `while let Some(event) = events.recv().await`.

```rust
// Thin wrapper over the engine.v1.EngineService this stack already runs — same
// client-construction pattern as services/tool/src/tools/kb_client.rs uses for knowledge-base.

use common::proto::engine::v1::{
    EngineServiceClient, ExecutionEvent, InterruptRequest, StartExecutionRequest,
    StreamEventsRequest,
};
use connectrpc::Protocol;
use connectrpc::client::{ClientConfig, HttpClient};
use std::time::Duration;
use tokio::sync::mpsc;
use uuid::Uuid;

const CALL_TIMEOUT: Duration = Duration::from_secs(20);

pub struct EngineClient {
    inner: EngineServiceClient<HttpClient>,
}

impl EngineClient {
    pub fn new(engine_url: &str) -> Result<Self, String> {
        let target = engine_url
            .parse()
            .map_err(|e| format!("could not parse ENGINE_URL {engine_url:?}: {e}"))?;
        Ok(Self {
            inner: EngineServiceClient::new(
                HttpClient::plaintext_http2_only(),
                ClientConfig::new(target)
                    .with_protocol(Protocol::Grpc)
                    .with_default_timeout(CALL_TIMEOUT)
                    .proto(),
            ),
        })
    }

    pub async fn start_execution(&self, graph_id: &str, input_json: &str) -> Result<Uuid, String> {
        let response = self
            .inner
            .start_execution(StartExecutionRequest {
                graph_id: graph_id.to_owned(),
                input_json: input_json.to_owned(),
                ..Default::default()
            })
            .await
            .map_err(|e| e.to_string())?
            .into_owned();
        Uuid::parse_str(&response.execution_id).map_err(|e| e.to_string())
    }

    pub async fn interrupt(&self, execution_id: Uuid, input_json: &str) -> Result<(), String> {
        self.inner
            .interrupt(InterruptRequest {
                execution_id: execution_id.to_string(),
                input_json: input_json.to_owned(),
            })
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Opens the stream, then drains it in a spawned task forwarding each event through the
    /// returned channel — the receiver ends (`recv()` returns `None`) when the stream ends
    /// cleanly, errors, or every receiver is dropped, whichever comes first.
    pub async fn stream_events(
        &self,
        execution_id: Uuid,
    ) -> Result<mpsc::UnboundedReceiver<ExecutionEvent>, String> {
        let mut stream = self
            .inner
            .stream_events(StreamEventsRequest {
                execution_id: Some(execution_id.to_string()),
                user_id: None,
            })
            .await
            .map_err(|e| e.to_string())?;
        let (sender, receiver) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            loop {
                match stream.message::<ExecutionEvent>().await {
                    Ok(Some(item)) => {
                        if sender.send(item.to_owned_message()).is_err() {
                            break; // receiver dropped — nobody's listening anymore
                        }
                    }
                    Ok(None) => break, // clean end of stream
                    Err(error) => {
                        tracing::error!(%error, "engine event stream error");
                        break;
                    }
                }
            }
        });
        Ok(receiver)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::proto::engine::v1::{
        CancelRequest, CancelResponse, CreateScheduleRequest, CreateScheduleResponse,
        EngineService, Execution, GetExecutionRequest, InterruptResponse, ListSchedulesRequest,
        ListSchedulesResponse, RegisterGraphRequest, RegisterGraphResponse, ResumeRequest,
        ResumeResponse, StartExecutionResponse,
    };
    use connectrpc::{
        RequestContext, Response, Router as ConnectRouter, ServiceRequest, ServiceResult,
        ServiceStream,
    };
    use std::sync::{Arc, Mutex};

    struct FakeEngine {
        execution_id: String,
        received_interrupts: Mutex<Vec<InterruptRequest>>,
    }

    #[allow(refining_impl_trait)]
    impl EngineService for FakeEngine {
        async fn register_graph(
            &self,
            _ctx: RequestContext,
            _request: ServiceRequest<'_, RegisterGraphRequest>,
        ) -> ServiceResult<RegisterGraphResponse> {
            Response::ok(RegisterGraphResponse::default())
        }
        async fn start_execution(
            &self,
            _ctx: RequestContext,
            _request: ServiceRequest<'_, StartExecutionRequest>,
        ) -> ServiceResult<StartExecutionResponse> {
            Response::ok(StartExecutionResponse {
                execution_id: self.execution_id.clone(),
            })
        }
        async fn interrupt(
            &self,
            _ctx: RequestContext,
            request: ServiceRequest<'_, InterruptRequest>,
        ) -> ServiceResult<InterruptResponse> {
            self.received_interrupts
                .lock()
                .expect("lock")
                .push(request.to_owned_message());
            Response::ok(InterruptResponse {})
        }
        async fn resume(
            &self,
            _ctx: RequestContext,
            _request: ServiceRequest<'_, ResumeRequest>,
        ) -> ServiceResult<ResumeResponse> {
            Response::ok(ResumeResponse {})
        }
        async fn cancel(
            &self,
            _ctx: RequestContext,
            _request: ServiceRequest<'_, CancelRequest>,
        ) -> ServiceResult<CancelResponse> {
            Response::ok(CancelResponse {})
        }
        async fn get_execution(
            &self,
            _ctx: RequestContext,
            _request: ServiceRequest<'_, GetExecutionRequest>,
        ) -> ServiceResult<Execution> {
            Response::ok(Execution::default())
        }
        async fn stream_events(
            &self,
            _ctx: RequestContext,
            _request: ServiceRequest<'_, StreamEventsRequest>,
        ) -> ServiceStream<ExecutionEvent> {
            Response::stream_ok(futures::stream::iter([Ok(ExecutionEvent {
                id: "e1".to_owned(),
                execution_id: self.execution_id.clone(),
                payload_kind: "ExecutionCompleted".to_owned(),
                payload_json: r#"{"final_state":{}}"#.to_owned(),
                ..Default::default()
            })]))
        }
        async fn create_schedule(
            &self,
            _ctx: RequestContext,
            _request: ServiceRequest<'_, CreateScheduleRequest>,
        ) -> ServiceResult<CreateScheduleResponse> {
            Response::ok(CreateScheduleResponse::default())
        }
        async fn list_schedules(
            &self,
            _ctx: RequestContext,
            _request: ServiceRequest<'_, ListSchedulesRequest>,
        ) -> ServiceResult<ListSchedulesResponse> {
            Response::ok(ListSchedulesResponse::default())
        }
    }

    async fn serve(fake: Arc<FakeEngine>) -> String {
        let connect = ConnectRouter::new().add_service(fake);
        let app = axum::Router::new().fallback_service(connect.into_axum_service());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("addr");
        tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
        format!("http://{address}")
    }

    #[tokio::test]
    async fn start_execution_returns_the_parsed_execution_id() {
        let execution_id = Uuid::new_v4();
        let fake = Arc::new(FakeEngine {
            execution_id: execution_id.to_string(),
            received_interrupts: Mutex::new(Vec::new()),
        });
        let url = serve(fake).await;
        let client = EngineClient::new(&url).expect("client");

        let got = client
            .start_execution("agent", r#"{"question": "hi"}"#)
            .await
            .expect("start");

        assert_eq!(got, execution_id);
    }

    #[tokio::test]
    async fn interrupt_sends_the_execution_id_and_input() {
        let execution_id = Uuid::new_v4();
        let fake = Arc::new(FakeEngine {
            execution_id: execution_id.to_string(),
            received_interrupts: Mutex::new(Vec::new()),
        });
        let url = serve(Arc::clone(&fake)).await;
        let client = EngineClient::new(&url).expect("client");

        client
            .interrupt(execution_id, r#"{"question": "more"}"#)
            .await
            .expect("interrupt");

        let received = fake.received_interrupts.lock().expect("lock");
        assert_eq!(received[0].execution_id, execution_id.to_string());
        assert_eq!(received[0].input_json, r#"{"question": "more"}"#);
    }

    #[tokio::test]
    async fn stream_events_yields_the_engine_stream() {
        let execution_id = Uuid::new_v4();
        let fake = Arc::new(FakeEngine {
            execution_id: execution_id.to_string(),
            received_interrupts: Mutex::new(Vec::new()),
        });
        let url = serve(fake).await;
        let client = EngineClient::new(&url).expect("client");

        let mut events = client.stream_events(execution_id).await.expect("stream");
        let event = events.recv().await.expect("one event");

        assert_eq!(event.payload_kind, "ExecutionCompleted");
        assert!(events.recv().await.is_none(), "stream ends cleanly after one event");
    }
}
```

Add `pub mod engine_client;` to `services/chat/src/lib.rs`. No new dependency needed for this
task — `EngineClient::stream_events` above returns a plain `tokio::sync::mpsc::UnboundedReceiver`,
not a `futures::Stream`, so it needs nothing beyond `tokio`'s `sync` feature, already added in
Task 1's `Cargo.toml`.

- [ ] **Step 2: Run**

Run: `cargo test -p chat engine_client:: 2>&1 | tail -30`
Expected: PASS, 3 tests. No Docker, no Engine running — a fake server on loopback.

- [ ] **Step 3: Commit**

```bash
git add services/chat/src/engine_client.rs services/chat/src/lib.rs services/chat/Cargo.toml
git commit -m "chat: EngineClient — StartExecution/Interrupt/StreamEvents adapter"
```

---

## Task 6: `topic_manager.rs` — `create_topic`, concurrency limit + queue

**Files:**
- Create: `services/chat/src/topic_manager.rs`
- Modify: `services/chat/src/lib.rs`

**Interfaces:**
- Consumes: `SessionManager` (Task 3), `EventBus` (Task 4), `EngineClient` (Task 5),
  `entity::topic` (Task 1).
- Produces: `TopicManager { session: SessionManager, engine: EngineClient, events: EventBus }`
  with `new(db, engine_url) -> Result<Self, String>`, `async fn create_topic(&self, user_id:
  Uuid, parent_id: Option<i64>, title: String, input_json: String) -> Result<(i64, topic::Status),
  ChatError>` — Task 8 adds `set_focus`/`send_turn`/the topic-watching consumer to the same
  `impl TopicManager`.

**Concurrency policy:** `MAX_CONCURRENT_TOPICS = 3` per session (spec's own stated default). A
new topic beyond the limit is inserted `Queued` (no Engine execution started yet); Task 8's
completion handler promotes the oldest `Queued` topic when a slot frees.

- [ ] **Step 1: Write the failing tests**

`services/chat/src/topic_manager.rs`:
```rust
// create_topic: the entry point for a new topic, root or child. Concurrency-limited per
// session (spec's default of 3 running at once) — beyond the limit, a topic is Queued instead
// of started; Task 8's completion handler promotes the oldest Queued topic when a slot frees.

use chrono::Utc;
use common::proto::chat::v1::ChatEvent;
use sea_orm::{ActiveValue::Set, ColumnTrait, DatabaseConnection, EntityTrait, PaginatorTrait, QueryFilter};
use uuid::Uuid;

use crate::engine_client::EngineClient;
use crate::entity::topic::{self, Status};
use crate::error::ChatError;
use crate::events::EventBus;
use crate::session_manager::SessionManager;

const AGENT_GRAPH_ID: &str = "agent";
pub const MAX_CONCURRENT_TOPICS: u64 = 3;

pub struct TopicManager {
    pub(crate) session: SessionManager,
    pub(crate) engine: EngineClient,
    pub(crate) events: EventBus,
}

impl TopicManager {
    pub fn new(db: DatabaseConnection, engine_url: &str) -> Result<Self, String> {
        Ok(Self {
            session: SessionManager::new(db),
            engine: EngineClient::new(engine_url)?,
            events: EventBus::default(),
        })
    }

    pub async fn create_topic(
        &self,
        user_id: Uuid,
        parent_id: Option<i64>,
        title: String,
        input_json: String,
    ) -> Result<(i64, Status), ChatError> {
        let session = self.session.get_or_create_session(user_id).await?;
        let running = topic::Entity::find()
            .filter(topic::Column::SessionId.eq(session.id))
            .filter(topic::Column::Status.eq(Status::Running))
            .count(&self.session.db)
            .await?;

        let now = Utc::now();
        let status = if running < MAX_CONCURRENT_TOPICS {
            Status::Running
        } else {
            Status::Queued
        };
        let execution_id = if status == Status::Running {
            Some(
                self.engine
                    .start_execution(AGENT_GRAPH_ID, &input_json)
                    .await
                    .map_err(ChatError::Engine)?,
            )
        } else {
            None
        };

        let row = topic::ActiveModel {
            session_id: Set(session.id),
            parent_id: Set(parent_id),
            title: Set(title),
            status: Set(status),
            execution_id: Set(execution_id),
            result_summary: Set(None),
            artifact_ids: Set(serde_json::json!([])),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(&self.session.db)
        .await?;

        if session.focus_topic_id.is_none() {
            self.set_focus_silently(session.id, row.id).await?;
        }

        self.events.publish(ChatEvent {
            topic_id: row.id.to_string(),
            kind: if status == Status::Running {
                "topic_started".to_owned()
            } else {
                "topic_queued".to_owned()
            }
            .to_owned(),
            occurred_at: now.to_rfc3339(),
            ..Default::default()
        });

        Ok((row.id, status))
    }

    /// Used only by `create_topic` for the "first topic in a session becomes focus" rule (spec)
    /// — doesn't emit `focus_changed` itself (there was no prior focus to change from). Task 8's
    /// `set_focus` is the user-facing operation and does emit it.
    async fn set_focus_silently(&self, session_id: Uuid, topic_id: i64) -> Result<(), ChatError> {
        use crate::entity::session;
        let mut active: session::ActiveModel = session::Entity::find_by_id(session_id)
            .one(&self.session.db)
            .await?
            .ok_or(ChatError::InvalidRequest("session vanished".to_owned()))?
            .into();
        active.focus_topic_id = Set(Some(topic_id));
        active.update(&self.session.db).await?;
        Ok(())
    }
}

#[cfg(all(test, feature = "test-support"))]
mod tests {
    use super::*;
    use common::proto::engine::v1::{
        CancelRequest, CancelResponse, CreateScheduleRequest, CreateScheduleResponse,
        EngineService, Execution, ExecutionEvent, GetExecutionRequest, InterruptRequest,
        InterruptResponse, ListSchedulesRequest, ListSchedulesResponse, RegisterGraphRequest,
        RegisterGraphResponse, ResumeRequest, ResumeResponse, StartExecutionRequest,
        StartExecutionResponse, StreamEventsRequest as EngineStreamEventsRequest,
    };
    use connectrpc::{
        RequestContext, Response, Router as ConnectRouter, ServiceRequest, ServiceResult,
        ServiceStream,
    };
    use std::sync::Arc;

    struct FakeEngine;

    #[allow(refining_impl_trait)]
    impl EngineService for FakeEngine {
        async fn register_graph(
            &self,
            _ctx: RequestContext,
            _request: ServiceRequest<'_, RegisterGraphRequest>,
        ) -> ServiceResult<RegisterGraphResponse> {
            Response::ok(RegisterGraphResponse::default())
        }
        async fn start_execution(
            &self,
            _ctx: RequestContext,
            _request: ServiceRequest<'_, StartExecutionRequest>,
        ) -> ServiceResult<StartExecutionResponse> {
            Response::ok(StartExecutionResponse {
                execution_id: uuid::Uuid::new_v4().to_string(),
            })
        }
        async fn interrupt(
            &self,
            _ctx: RequestContext,
            _request: ServiceRequest<'_, InterruptRequest>,
        ) -> ServiceResult<InterruptResponse> {
            Response::ok(InterruptResponse {})
        }
        async fn resume(
            &self,
            _ctx: RequestContext,
            _request: ServiceRequest<'_, ResumeRequest>,
        ) -> ServiceResult<ResumeResponse> {
            Response::ok(ResumeResponse {})
        }
        async fn cancel(
            &self,
            _ctx: RequestContext,
            _request: ServiceRequest<'_, CancelRequest>,
        ) -> ServiceResult<CancelResponse> {
            Response::ok(CancelResponse {})
        }
        async fn get_execution(
            &self,
            _ctx: RequestContext,
            _request: ServiceRequest<'_, GetExecutionRequest>,
        ) -> ServiceResult<Execution> {
            Response::ok(Execution::default())
        }
        async fn stream_events(
            &self,
            _ctx: RequestContext,
            _request: ServiceRequest<'_, EngineStreamEventsRequest>,
        ) -> ServiceStream<ExecutionEvent> {
            Response::stream_ok(futures::stream::empty())
        }
        async fn create_schedule(
            &self,
            _ctx: RequestContext,
            _request: ServiceRequest<'_, CreateScheduleRequest>,
        ) -> ServiceResult<CreateScheduleResponse> {
            Response::ok(CreateScheduleResponse::default())
        }
        async fn list_schedules(
            &self,
            _ctx: RequestContext,
            _request: ServiceRequest<'_, ListSchedulesRequest>,
        ) -> ServiceResult<ListSchedulesResponse> {
            Response::ok(ListSchedulesResponse::default())
        }
    }

    async fn serve_engine() -> String {
        let connect = ConnectRouter::new().add_service(Arc::new(FakeEngine));
        let app = axum::Router::new().fallback_service(connect.into_axum_service());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("addr");
        tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
        format!("http://{address}")
    }

    async fn manager_with() -> (crate::test_db::TestDb, TopicManager) {
        let test = crate::test_db::start().await;
        let engine_url = serve_engine().await;
        let manager = TopicManager::new(test.db.clone(), &engine_url).expect("manager");
        (test, manager)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_first_topic_in_a_session_starts_running_and_becomes_focus() {
        let (_test, manager) = manager_with().await;
        let user_id = Uuid::new_v4();

        let (topic_id, status) = manager
            .create_topic(user_id, None, "First".into(), r#"{"question": "hi"}"#.into())
            .await
            .expect("create");

        assert_eq!(status, Status::Running);
        let (focus, _topics) = manager.session.get_session_view(user_id).await.expect("view");
        assert_eq!(focus, Some(topic_id));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_fourth_concurrent_topic_is_queued_not_started() {
        let (_test, manager) = manager_with().await;
        let user_id = Uuid::new_v4();
        for i in 0..MAX_CONCURRENT_TOPICS {
            let (_id, status) = manager
                .create_topic(user_id, None, format!("T{i}"), "{}".into())
                .await
                .expect("create");
            assert_eq!(status, Status::Running);
        }

        let (_id, status) = manager
            .create_topic(user_id, None, "Overflow".into(), "{}".into())
            .await
            .expect("create");

        assert_eq!(status, Status::Queued);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_second_root_topic_does_not_steal_focus() {
        let (_test, manager) = manager_with().await;
        let user_id = Uuid::new_v4();
        let (first_id, _) = manager
            .create_topic(user_id, None, "First".into(), "{}".into())
            .await
            .expect("create");

        manager
            .create_topic(user_id, None, "Second".into(), "{}".into())
            .await
            .expect("create");

        let (focus, _topics) = manager.session.get_session_view(user_id).await.expect("view");
        assert_eq!(focus, Some(first_id));
    }
}
```

Add `pub mod topic_manager;` to `services/chat/src/lib.rs`.

- [ ] **Step 2: Run**

Run: `cargo test -p chat --features test-support topic_manager:: 2>&1 | tail -40`
Requires Docker (testcontainers). Expected: PASS, 3 tests.

- [ ] **Step 3: Commit**

```bash
git add services/chat/src/topic_manager.rs services/chat/src/lib.rs
git commit -m "chat: TopicManager — create_topic, concurrency limit + queue"
```

---

## Task 7: `topic_manager.rs` — `set_focus`, `send_turn`, the completion consumer

**Files:**
- Modify: `services/chat/src/topic_manager.rs`

**Interfaces:**
- Consumes: Task 6's `TopicManager`, `entity::message` (Task 1).
- Produces: `async fn set_focus(&self, user_id: Uuid, topic_id: i64) -> Result<(), ChatError>`,
  `async fn send_turn(&self, user_id: Uuid, turn_id: Uuid, content: String) -> Result<i64,
  ChatError>`, `async fn watch_topic(&self, topic_id: i64, execution_id: Uuid)` (spawned by
  `create_topic`/the promotion path — a background task, not called by the RPC layer directly),
  `async fn recover(&self)` — Task 9's `main.rs` calls `recover` once at startup and spawns
  `watch_topic` wherever `create_topic` (Task 6) or the promotion logic here starts a topic
  Running.

- [ ] **Step 1: Write the failing tests — extend `topic_manager.rs`'s existing `impl`/tests**

Add to `services/chat/src/topic_manager.rs`'s `impl TopicManager` block (after `create_topic`):
```rust
    pub async fn set_focus(&self, user_id: Uuid, topic_id: i64) -> Result<(), ChatError> {
        let session = self.session.get_or_create_session(user_id).await?;
        let topic = topic::Entity::find_by_id(topic_id)
            .one(&self.session.db)
            .await?
            .filter(|t| t.session_id == session.id)
            .ok_or(ChatError::TopicNotFound(topic_id))?;

        use crate::entity::session;
        let previous = session.focus_topic_id;
        let mut active: session::ActiveModel = session::Entity::find_by_id(session.id)
            .one(&self.session.db)
            .await?
            .ok_or(ChatError::InvalidRequest("session vanished".to_owned()))?
            .into();
        active.focus_topic_id = Set(Some(topic_id));
        active.update(&self.session.db).await?;

        self.events.publish(ChatEvent {
            topic_id: topic.id.to_string(),
            kind: "focus_changed".to_owned(),
            payload_json: serde_json::json!({"from": previous, "reason": "user"}).to_string(),
            occurred_at: Utc::now().to_rfc3339(),
        });
        Ok(())
    }

    pub async fn send_turn(
        &self,
        user_id: Uuid,
        turn_id: Uuid,
        content: String,
    ) -> Result<i64, ChatError> {
        let (focus_topic_id, _topics) = self.session.get_session_view(user_id).await?;
        let topic_id = focus_topic_id.ok_or(ChatError::NoFocus)?;
        let topic = topic::Entity::find_by_id(topic_id)
            .one(&self.session.db)
            .await?
            .ok_or(ChatError::TopicNotFound(topic_id))?;

        use crate::entity::message;
        if message::Entity::find()
            .filter(message::Column::TopicId.eq(topic_id))
            .filter(message::Column::TurnId.eq(turn_id))
            .one(&self.session.db)
            .await?
            .is_some()
        {
            return Ok(topic_id); // already applied — idempotent no-op
        }
        message::ActiveModel {
            topic_id: Set(topic_id),
            turn_id: Set(turn_id),
            content: Set(content.clone()),
            created_at: Set(Utc::now()),
            ..Default::default()
        }
        .insert(&self.session.db)
        .await?;

        let execution_id = topic
            .execution_id
            .ok_or(ChatError::InvalidRequest(format!(
                "topic {topic_id} has no running execution to interrupt"
            )))?;
        let input_json = serde_json::json!({"question": content}).to_string();
        self.engine
            .interrupt(execution_id, &input_json)
            .await
            .map_err(ChatError::Engine)?;
        Ok(topic_id)
    }

    /// Consumes one topic's Engine event stream to completion, updating `chat.topics` and
    /// publishing to the `EventBus` as it goes. Spawned as a background task whenever a topic
    /// starts Running — from `create_topic`, from `promote_next_queued` below, or from
    /// `recover` at startup. Never returns an error to a caller: a stream failure is logged and
    /// the topic is left as-is (recoverable by `recover` on the next restart).
    pub async fn watch_topic(&self, topic_id: i64, execution_id: Uuid) {
        let mut events = match self.engine.stream_events(execution_id).await {
            Ok(events) => events,
            Err(error) => {
                tracing::error!(topic_id, %error, "failed to open Engine event stream");
                return;
            }
        };
        while let Some(event) = events.recv().await {
            if let Err(error) = self.handle_engine_event(topic_id, &event).await {
                tracing::error!(topic_id, %error, "failed to handle an Engine event");
            }
        }
    }

    async fn handle_engine_event(
        &self,
        topic_id: i64,
        event: &common::proto::engine::v1::ExecutionEvent,
    ) -> Result<(), ChatError> {
        match event.payload_kind.as_str() {
            "ExecutionCompleted" => self.finish_topic(topic_id, Status::Completed, event).await,
            "ExecutionFailed" => self.finish_topic(topic_id, Status::Failed, event).await,
            _ => {
                self.events.publish(ChatEvent {
                    topic_id: topic_id.to_string(),
                    kind: "topic_progress".to_owned(),
                    payload_json: event.payload_json.clone(),
                    occurred_at: event.occurred_at.clone(),
                });
                Ok(())
            }
        }
    }

    async fn finish_topic(
        &self,
        topic_id: i64,
        status: Status,
        event: &common::proto::engine::v1::ExecutionEvent,
    ) -> Result<(), ChatError> {
        let payload: serde_json::Value =
            serde_json::from_str(&event.payload_json).unwrap_or(serde_json::Value::Null);
        let summary = payload
            .get("final_state")
            .and_then(|state| state.pointer("/llm/reply"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .or_else(|| payload.get("error").and_then(serde_json::Value::as_str).map(str::to_owned));

        let mut active: topic::ActiveModel = topic::Entity::find_by_id(topic_id)
            .one(&self.session.db)
            .await?
            .ok_or(ChatError::TopicNotFound(topic_id))?
            .into();
        active.status = Set(status);
        active.result_summary = Set(summary.clone());
        active.updated_at = Set(Utc::now());
        let row = active.update(&self.session.db).await?;

        let kind = if status == Status::Completed {
            "topic_completed"
        } else {
            "topic_failed"
        };
        self.events.publish(ChatEvent {
            topic_id: topic_id.to_string(),
            kind: kind.to_owned(),
            payload_json: serde_json::json!({"summary": summary}).to_string(),
            occurred_at: row.updated_at.to_rfc3339(),
        });
        self.events.publish(ChatEvent {
            topic_id: topic_id.to_string(),
            kind: "notification".to_owned(),
            payload_json: serde_json::json!({
                "kind": if status == Status::Completed { "completed" } else { "failed" },
                "text": summary,
            })
            .to_string(),
            occurred_at: row.updated_at.to_rfc3339(),
        });

        self.promote_next_queued(row.session_id).await
    }

    /// Called after any topic leaves `Running` — starts the oldest still-`Queued` topic in the
    /// same session, if the slot that just freed leaves room (it always does: one topic just
    /// left `Running`). Returns `Ok(())` with nothing promoted if there's no queued topic.
    async fn promote_next_queued(&self, session_id: Uuid) -> Result<(), ChatError> {
        let Some(next) = topic::Entity::find()
            .filter(topic::Column::SessionId.eq(session_id))
            .filter(topic::Column::Status.eq(Status::Queued))
            .order_by_asc(topic::Column::CreatedAt)
            .one(&self.session.db)
            .await?
        else {
            return Ok(());
        };
        let input_json = "{}".to_owned(); // the queued topic's own creation input is not re-sent; see Task 7's note below
        let execution_id = self
            .engine
            .start_execution("agent", &input_json)
            .await
            .map_err(ChatError::Engine)?;
        let mut active: topic::ActiveModel = next.clone().into();
        active.status = Set(Status::Running);
        active.execution_id = Set(Some(execution_id));
        active.updated_at = Set(Utc::now());
        active.update(&self.session.db).await?;

        self.events.publish(ChatEvent {
            topic_id: next.id.to_string(),
            kind: "topic_started".to_owned(),
            occurred_at: Utc::now().to_rfc3339(),
            ..Default::default()
        });
        Ok(())
    }

    /// Called once at startup (Task 9's `main.rs`) — re-attaches a live `watch_topic` consumer
    /// to every topic this process left `Running` before a restart. Recovery correctness relies
    /// on Engine's `StreamEvents(execution_id)` replaying full history from version 1 (not just
    /// new events from the subscribe point) — confirmed against `services/engine`'s own
    /// `StreamEvents` handler; if this ever changes, `recover` needs a version cursor too.
    pub async fn recover(self: &std::sync::Arc<Self>) -> Result<(), ChatError> {
        let running = topic::Entity::find()
            .filter(topic::Column::Status.eq(Status::Running))
            .all(&self.session.db)
            .await?;
        for topic in running {
            let Some(execution_id) = topic.execution_id else {
                continue;
            };
            let manager = std::sync::Arc::clone(self);
            tokio::spawn(async move { manager.watch_topic(topic.id, execution_id).await });
        }
        Ok(())
    }
```

Update `TopicManager::create_topic` (Task 6) to also spawn `watch_topic` when it starts a topic
Running: after inserting the row, if `status == Status::Running`, spawn
`tokio::spawn(async move { manager.watch_topic(row.id, execution_id).await })` — this requires
`create_topic` to take `self: &std::sync::Arc<Self>` instead of `&self` (update its signature and
every existing test's call site accordingly: `Arc::new(manager).create_topic(...)`). Also update
`promote_next_queued` (this task) to spawn its own `watch_topic` the same way — it needs
`self: &std::sync::Arc<Self>` too, so give both a shared private helper `fn spawn_watch(self:
&std::sync::Arc<Self>, topic_id: i64, execution_id: Uuid)` that does the `tokio::spawn` line
once, called from both `create_topic` and `promote_next_queued`.

**Note on `promote_next_queued`'s `input_json`:** the spec doesn't define what a queued topic
resumes with once promoted — its original `CreateTopicRequest.input_json` isn't persisted
anywhere in this plan's schema (Task 1's `topic` entity has no `input_json` column). This is a
real gap worth flagging rather than silently guessing: for this scope, promotion starts the
topic's execution with an empty `{}` initial state, which means a queued topic's original intent
(its creation prompt) is lost by the time it actually runs. **This is acceptable for tonight's
scope** (the spec's own e2e scenario never exercises the queue — 2 topics, limit is 3) but is a
real limitation for anyone hitting the queue in practice. Add a one-line code comment at
`promote_next_queued`'s `input_json` saying exactly this, and leave a `// TODO` only in the sense
of a comment — do not attempt to retrofit an `input_json` column into the entity as part of this
task; that would be solving a problem outside this task's brief. (If a future task adds it, that
task also needs a migration-safe default for existing rows — not this one's problem.)

- [ ] **Step 2: Run**

Run: `cargo test -p chat --features test-support topic_manager:: 2>&1 | tail -60`
Requires Docker. Expected: PASS — the 3 existing tests from Task 6 still pass (after their
`Arc::new(manager)` call-site update), plus new tests for `set_focus` (changes the pointer,
emits `focus_changed`), `send_turn` (routes to focus, interrupts Engine, is a no-op on a repeated
`turn_id`), and `finish_topic`/`promote_next_queued` (a completed topic's `Queued` sibling gets
promoted to `Running`) — write these following the existing tests' fake-Engine pattern.

- [ ] **Step 3: Commit**

```bash
git add services/chat/src/topic_manager.rs
git commit -m "chat: TopicManager — set_focus, send_turn, the completion consumer, recovery"
```

---

## Task 8: `main.rs` — Connect trait impl, the running service

**Files:**
- Modify: `services/chat/src/main.rs` (replaces Task 1's placeholder entirely)

**Interfaces:**
- Consumes: everything built in Tasks 1-7.
- Produces: a running binary on port `8088` (the next free port after Tool Service's `8087`) —
  no new public Rust API, this is the assembly point. Verified by Task 10's `docker compose up`
  + curl, not a unit test.

- [ ] **Step 1: Write `main.rs`**

```rust
// chat: session/topic orchestration over Engine. Connects to Postgres, syncs its schema,
// recovers any topics left Running from a prior process, serves Connect RPC.

use std::sync::Arc;

use axum::routing::get;
use chat::entity::topic::Status;
use chat::error::ChatError;
use chat::topic_manager::TopicManager;
use common::proto::chat::v1::{
    ChatEvent, ChatService, CreateTopicRequest, CreateTopicResponse, GetSessionRequest,
    GetSessionResponse, SendTurnRequest, SendTurnResponse, SetFocusRequest, SetFocusResponse,
    StreamEventsRequest, Topic as TopicProto,
};
use connectrpc::{
    ConnectError, RequestContext, Response, Router as ConnectRouter, ServiceRequest,
    ServiceResult, ServiceStream,
};
use futures::StreamExt;
use sea_orm::Database;
use uuid::Uuid;

const CHAT_PORT: &str = "0.0.0.0:8088";

fn env(name: &str) -> Result<String, String> {
    std::env::var(name).map_err(|_| format!("{name} is not set"))
}

fn status_to_str(status: Status) -> &'static str {
    match status {
        Status::Queued => "queued",
        Status::Running => "running",
        Status::Completed => "completed",
        Status::Failed => "failed",
        Status::Cancelled => "cancelled",
    }
}

fn require_principal(ctx: &RequestContext) -> Result<Uuid, ConnectError> {
    common::principal::from_metadata(ctx.headers())
        .map(|p| p.user_id)
        .ok_or_else(|| ConnectError::invalid_argument("request needs a Principal in the metadata"))
}

struct ChatServiceImpl {
    topics: Arc<TopicManager>,
}

#[allow(refining_impl_trait)]
impl ChatService for ChatServiceImpl {
    async fn create_topic(
        &self,
        ctx: RequestContext,
        request: ServiceRequest<'_, CreateTopicRequest>,
    ) -> ServiceResult<CreateTopicResponse> {
        let user_id = require_principal(&ctx)?;
        let msg = request.to_owned_message();
        let parent_id = msg
            .parent_id
            .map(|id| id.parse::<i64>())
            .transpose()
            .map_err(|_| ConnectError::invalid_argument("parent_id is not a valid id"))?;
        let (topic_id, status) = self
            .topics
            .create_topic(user_id, parent_id, msg.title, msg.input_json)
            .await?;
        Response::ok(CreateTopicResponse {
            topic_id: topic_id.to_string(),
            status: status_to_str(status).to_owned(),
        })
    }

    async fn set_focus(
        &self,
        ctx: RequestContext,
        request: ServiceRequest<'_, SetFocusRequest>,
    ) -> ServiceResult<SetFocusResponse> {
        let user_id = require_principal(&ctx)?;
        let msg = request.to_owned_message();
        let topic_id: i64 = msg
            .topic_id
            .parse()
            .map_err(|_| ConnectError::invalid_argument("topic_id is not a valid id"))?;
        self.topics.set_focus(user_id, topic_id).await?;
        Response::ok(SetFocusResponse {})
    }

    async fn send_turn(
        &self,
        ctx: RequestContext,
        request: ServiceRequest<'_, SendTurnRequest>,
    ) -> ServiceResult<SendTurnResponse> {
        let user_id = require_principal(&ctx)?;
        let msg = request.to_owned_message();
        let turn_id = Uuid::parse_str(&msg.turn_id)
            .map_err(|_| ConnectError::invalid_argument("turn_id is not a valid uuid"))?;
        let topic_id = self.topics.send_turn(user_id, turn_id, msg.content).await?;
        Response::ok(SendTurnResponse {
            topic_id: topic_id.to_string(),
        })
    }

    async fn get_session(
        &self,
        ctx: RequestContext,
        _request: ServiceRequest<'_, GetSessionRequest>,
    ) -> ServiceResult<GetSessionResponse> {
        let user_id = require_principal(&ctx)?;
        let (focus_topic_id, topics) = self
            .topics
            .session
            .get_session_view(user_id)
            .await
            .map_err(ChatError::from)?;
        Response::ok(GetSessionResponse {
            focus_topic_id: focus_topic_id.map(|id| id.to_string()),
            topics: topics
                .into_iter()
                .map(|t| TopicProto {
                    id: t.id.to_string(),
                    parent_id: t.parent_id.map(|id| id.to_string()),
                    title: t.title,
                    status: status_to_str(t.status).to_owned(),
                    execution_id: t.execution_id.map(|id| id.to_string()),
                    result_summary: t.result_summary,
                })
                .collect(),
        })
    }

    async fn stream_events(
        &self,
        ctx: RequestContext,
        _request: ServiceRequest<'_, StreamEventsRequest>,
    ) -> ServiceStream<ChatEvent> {
        let user_id = require_principal(&ctx)?;
        let (_, topics) = self
            .topics
            .session
            .get_session_view(user_id)
            .await
            .map_err(ChatError::from)?;
        let snapshot = topics.into_iter().map(|t| ChatEvent {
            topic_id: t.id.to_string(),
            kind: "topic_created".to_owned(),
            payload_json: serde_json::json!({
                "title": t.title,
                "status": status_to_str(t.status),
                "parent_id": t.parent_id,
            })
            .to_string(),
            occurred_at: t.updated_at.to_rfc3339(),
        });
        let live = tokio_stream::wrappers::BroadcastStream::new(self.topics.events.subscribe())
            .filter_map(|item| async move { item.ok() });
        Response::stream_ok(futures::stream::iter(snapshot).chain(live))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    common::logging::init();

    let database_url = env("DATABASE_URL")?;
    let db = Database::connect(&database_url).await?;
    db.get_schema_registry("chat::entity::*").sync(&db).await?;

    let topics = Arc::new(TopicManager::new(db, &env("ENGINE_URL")?)?);
    topics.recover().await?;

    let chat_service = ChatServiceImpl {
        topics: Arc::clone(&topics),
    };
    let connect = ConnectRouter::new().add_service(Arc::new(chat_service));
    let app = axum::Router::new()
        .route("/health", get(|| async { "OK" }))
        .fallback_service(connect.into_axum_service());

    let listener = tokio::net::TcpListener::bind(CHAT_PORT).await?;
    tracing::info!("chat listening on {CHAT_PORT}");
    axum::serve(listener, app).await?;
    Ok(())
}
```

No new dependency needed for this task — `futures` and `tokio-stream` were already added to
`services/chat/Cargo.toml` in Task 1.

- [ ] **Step 2: Verify it compiles**

Run: `cargo check -p chat --all-features 2>&1 | tail-30`
Expected: compiles clean. Also run `cargo test -p chat --features test-support 2>&1 | tail -30`
to confirm every earlier task's tests still pass unchanged (this task only adds `main.rs`).

- [ ] **Step 3: Commit**

```bash
git add services/chat/src/main.rs services/chat/Cargo.toml Cargo.toml Cargo.lock
git commit -m "chat: main.rs — Connect trait impl, recovery, wire it all up"
```

---

## Task 9: Docker — Dockerfile, compose.yaml, Postgres role/schema, `.env.example`

**Files:**
- Create: `services/chat/Dockerfile`
- Modify: `compose.yaml`, `.env.example`

**Interfaces:** none — this task makes the service buildable, runnable in the stack, and proves
it with a real health check; the end-to-end proof of what it actually *does* is Task 11.

- [ ] **Step 1: `Dockerfile`, mirroring `services/tool/Dockerfile`**

`services/chat/Dockerfile`:
```dockerfile
# Chat image: compiles the session/topic orchestration service with cached dependencies and ships the static binary alone on Alpine.
FROM rust:1.98-alpine3.24 AS build
RUN apk add --no-cache protoc
WORKDIR /app
ENV CARGO_HOME=/var/cache/cargo CARGO_TARGET_DIR=/var/cache/target
RUN --mount=type=bind,target=. \
    --mount=type=cache,target=/var/cache/cargo \
    --mount=type=cache,target=/var/cache/target \
    cargo build --locked --release --package chat \
    && cp /var/cache/target/release/chat /usr/local/bin/

FROM alpine:3.24
COPY --from=build /usr/local/bin/chat /usr/local/bin/
USER 10001:10001
EXPOSE 8088
HEALTHCHECK --interval=5s CMD ["wget", "-qO-", "http://127.0.0.1:8088/health"]
CMD ["chat"]
```

- [ ] **Step 2: `compose.yaml` — Postgres role/schema**

Add to the `postgres-bootstrap` config's `content`, after the existing `tool_user` block:
```
      \getenv chat_password CHAT_DB_PASSWORD
      CREATE ROLE chat_user LOGIN PASSWORD :'chat_password';
      CREATE SCHEMA chat AUTHORIZATION chat_user;
      ALTER ROLE chat_user SET search_path TO chat, public;
```
Add to the `postgres` service's `environment` block, alongside the existing `*_DB_PASSWORD`
lines:
```yaml
      CHAT_DB_PASSWORD: ${CHAT_DB_PASSWORD:?set it in .env, see .env.example}
```

- [ ] **Step 3: `compose.yaml` — the `chat` service block**

```yaml
  chat:
    build:
      dockerfile: services/chat/Dockerfile
    develop: *rebuild-on-save
    environment:
      ENGINE_URL: http://engine:8085
      DATABASE_URL: postgres://chat_user:${CHAT_DB_PASSWORD:?set it in .env, see .env.example}@postgres:5432/${POSTGRES_DB:-app}
    depends_on:
      postgres:
        condition: service_healthy
      engine:
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
CHAT_DB_PASSWORD=change-me
```

- [ ] **Step 5: Bootstrap the live Postgres role/schema and bring the service up**

The bootstrap config in `compose.yaml` only runs against a fresh volume — this stack's Postgres
has been running for days, so create the role/schema by hand, the same way `tool_user`/`tool`
were created earlier tonight:
```bash
PW=$(openssl rand -hex 24)
printf '\n# chat: session/topic orchestration service (services/chat)\nCHAT_DB_PASSWORD=%s\n' "$PW" >> .env
docker compose exec -T -e PGPASSWORD="$(grep '^POSTGRES_PASSWORD=' .env | cut -d= -f2-)" postgres \
  psql -U "$(grep '^POSTGRES_USER=' .env | cut -d= -f2-)" -d "$(grep '^POSTGRES_DB=' .env | cut -d= -f2-)" \
  -v ON_ERROR_STOP=1 -v pw="$PW" <<'SQL'
DO $$ BEGIN IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname='chat_user') THEN CREATE ROLE chat_user LOGIN; END IF; END $$;
ALTER ROLE chat_user LOGIN PASSWORD :'pw';
CREATE SCHEMA IF NOT EXISTS chat AUTHORIZATION chat_user;
ALTER ROLE chat_user SET search_path TO chat, public;
SQL
```
Then:
```bash
timeout 10 docker ps
docker compose -p aiengineeringboilerplate build chat
docker compose -p aiengineeringboilerplate up -d --no-deps chat
docker compose -p aiengineeringboilerplate ps --format '{{.Service}} {{.Status}}'
docker exec aiengineeringboilerplate-chat-1 wget -qO- http://127.0.0.1:8088/health
```
Expected: `chat` shows `Up`/`healthy` alongside the other 7 services (8 total now); health check
returns `OK`.

Always pass `-p aiengineeringboilerplate` explicitly — a bare `docker compose` picks up the
current directory's basename as the project unless pinned.

- [ ] **Step 6: Commit**

```bash
git add services/chat/Dockerfile compose.yaml .env.example
git commit -m "chat: Docker — Dockerfile, compose service, Postgres role/schema, .env.example"
```

---

## Task 10: Gateway — proxy Chat's Connect surface

**Files:**
- Modify: `services/gateway/src/config.rs` (add `chat_url`)
- Modify: `services/gateway/src/proxy.rs` (add a `chat` client + `ChatService` impl)
- Modify: `services/gateway/src/app.rs` (wire the new client into `Gateway::new`, register the
  service on the Connect router)
- Modify: `services/gateway/Cargo.toml` (add `tokio-stream`, needed for `stream_events`'s
  channel-to-stream adapter)
- Modify: `compose.yaml` (the `gateway` service block gains `CHAT_URL`)
- Modify: `.env.example`

**Interfaces:**
- Consumes: `common::proto::chat::v1::{ChatService, ChatServiceClient, ...}` (Task 2).
- Produces: `Gateway` now proxies 3 backend services (`crawler`, `knowledge_base`, `chat`); no
  new public Rust API beyond the added field — a passthrough, no new logic.

- [ ] **Step 1: `config.rs` — add `chat_url`**

Mirror the existing `knowledge_base_url` field exactly: a `const DEFAULT_CHAT_URL: &str =
"http://127.0.0.1:8088"`, a `chat_url: Uri` field on `GatewayConfig`, a `pub fn chat_url(&self)
-> &Uri` accessor, the same `endpoint("CHAT_URL", read_or_default("CHAT_URL",
DEFAULT_CHAT_URL)?)` construction in the config-loading function, and the same addition to
`for_test`'s literal struct (`chat_url: Uri::from_static(DEFAULT_CHAT_URL)`).

- [ ] **Step 2: `proxy.rs` — a `chat` client, straight passthrough for every RPC**

Add `use common::proto::chat::v1::{...all 5 RPCs' request/response types plus ChatService,
ChatServiceClient...};` to the existing `use` block. Add `pub(crate) chat:
ChatServiceClient<HttpClient>` to the `Gateway` struct, constructed in `Gateway::new` the exact
same way `knowledge_base` is (a new `chat_url: Uri` parameter, `KNOWLEDGE_BASE_CALL_TIMEOUT`-style
constant e.g. `const CHAT_CALL_TIMEOUT: Duration = Duration::from_secs(10)`, no retry wrapper —
`chat`'s own RPCs aren't idempotent-safe to blind-retry the way `crawler`'s read-only ones are,
so match `KnowledgeBaseService`'s un-retried style, not `CrawlerService`'s retried one).

Add `#[allow(refining_impl_trait)] impl ChatService for Gateway` with all 5 methods. The four
unary ones are pure passthroughs, matching `KnowledgeBaseService`'s `search`/`read_document`
style exactly (forward `request.to_owned_message()`, `.await?.into_owned()`,
`Response::ok(upstream)`):

```rust
    async fn create_topic(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, CreateTopicRequest>,
    ) -> ServiceResult<CreateTopicResponse> {
        let upstream = self
            .chat
            .create_topic(request.to_owned_message())
            .await?
            .into_owned();
        Response::ok(upstream)
    }

    async fn set_focus(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, SetFocusRequest>,
    ) -> ServiceResult<SetFocusResponse> {
        let upstream = self
            .chat
            .set_focus(request.to_owned_message())
            .await?
            .into_owned();
        Response::ok(upstream)
    }

    async fn send_turn(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, SendTurnRequest>,
    ) -> ServiceResult<SendTurnResponse> {
        let upstream = self
            .chat
            .send_turn(request.to_owned_message())
            .await?
            .into_owned();
        Response::ok(upstream)
    }

    async fn get_session(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, GetSessionRequest>,
    ) -> ServiceResult<GetSessionResponse> {
        let upstream = self
            .chat
            .get_session(request.to_owned_message())
            .await?
            .into_owned();
        Response::ok(upstream)
    }
```

`stream_events` is this codebase's first server-to-server streaming passthrough. A client-side
`ServerStream` (what `self.chat.stream_events(...)` returns) is **not** a `futures::Stream` —
verified directly against the real generated client
(`target/debug/build/common-*/out/engine.v1.engine.__connect.rs`, `chat.v1.chat.__connect.rs`
after Task 2 generates it): it's consumed one message at a time via
`stream.message::<ChatEvent>().await`. The server-side return type (`ServiceStream<ChatEvent>`,
what `Response::stream_ok` needs) genuinely does need a real `futures::Stream` — Tool
Service/Engine's own handlers already establish that with `futures::stream::iter(...)`. The
same channel-based adapter `EngineClient::stream_events` (Task 5) uses bridges the two: drain the
client-side poll loop in a spawned task, forward each item through an unbounded channel, and wrap
the receiving end as a `Stream` via `tokio_stream::wrappers::UnboundedReceiverStream` (which
`services/chat`'s Task 8 already established as a real, working dependency for exactly this kind
of channel-to-stream conversion):

```rust
    async fn stream_events(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, StreamEventsRequest>,
    ) -> ServiceStream<ChatEvent> {
        let mut stream = self.chat.stream_events(request.to_owned_message()).await?;
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            loop {
                match stream.message::<ChatEvent>().await {
                    Ok(Some(item)) => {
                        if sender.send(Ok(item.to_owned_message())).is_err() {
                            break; // receiver dropped
                        }
                    }
                    Ok(None) => break, // clean end of stream
                    Err(error) => {
                        let _ = sender.send(Err(error));
                        break;
                    }
                }
            }
        });
        Response::stream_ok(tokio_stream::wrappers::UnboundedReceiverStream::new(receiver))
    }
```

Add `tokio-stream = { workspace = true, features = ["sync"] }` to `services/gateway/Cargo.toml`'s
`[dependencies]` — Gateway had no streaming RPCs before this task, so unlike `services/chat`
(which already added it in its own Task 1), this is Gateway's first need for it.

- [ ] **Step 3: `app.rs` — wire it up**

`Gateway::new` gains a `chat_url` parameter (update its one call site in `routes` to pass
`config.chat_url().clone()`). Add
`.add_service::<_, common::proto::chat::v1::ChatServiceRegisterMarker>(Arc::clone(&gateway))` to
the `ConnectRouter` chain in `routes`, matching the existing two `.add_service` calls' style
(the LAST `.add_service` call in the chain currently takes `gateway` by value, not
`Arc::clone(&gateway)`, since nothing needs it after — adding a third service means the first two
calls both need `Arc::clone(&gateway)` now, only the new last one takes `gateway` by value; check
the current file and adjust exactly one line, not all three).

- [ ] **Step 4: `compose.yaml` + `.env.example`**

Add `CHAT_URL: http://chat:8088` to the `gateway` service block's `environment`, alongside the
existing `CRAWLER_URL`/`KNOWLEDGE_BASE_URL` lines. Add `chat: condition: service_healthy` to
`gateway`'s `depends_on`. `.env.example` needs no new entry — `CHAT_URL` is a literal compose
value, not a `.env`-sourced secret, matching how `KNOWLEDGE_BASE_URL` is already handled for
`gateway`.

- [ ] **Step 5: Verify**

Run: `export PATH="$HOME/.cargo/bin:$PATH" && cargo check -p gateway --all-features 2>&1 | tail -30`
Expected: compiles clean. Run `cargo test -p gateway 2>&1 | tail -30` to confirm no regressions
in Gateway's existing test suite.

- [ ] **Step 6: Rebuild and restart `gateway` live**

```bash
timeout 10 docker ps
docker compose -p aiengineeringboilerplate build gateway
docker compose -p aiengineeringboilerplate up -d gateway
docker compose -p aiengineeringboilerplate ps gateway chat
```
Expected: both `healthy`. If `gateway` crash-loops, check `docker compose -p
aiengineeringboilerplate logs gateway --tail 50` — the most likely cause is `CHAT_URL` missing
from the container's actual environment.

- [ ] **Step 7: Commit**

```bash
git add services/gateway/src/config.rs services/gateway/src/proxy.rs \
        services/gateway/src/app.rs services/gateway/Cargo.toml Cargo.lock compose.yaml
git commit -m "gateway: proxy Chat Service's Connect surface"
```

---

## Task 11: Frontend — `chat.html`

**Files:**
- Create: `services/frontend/client/chat.html`
- Modify: `services/gateway/src/app.rs` (serve it, same pattern as `index.html`/`sources.html`)

**Interfaces:** none — a static page plus one new route.

- [ ] **Step 1: `chat.html`**

Matches `services/frontend/client/index.html`'s exact precedent: inline `<style>`, a top-of-file
`<meta charset>`/`<title>`, the same `.logout` form, vanilla JS, no framework, no build step. The
streaming envelope format below is verified directly against `connectrpc`'s own implementation
(`~/.cargo/registry/src/*/connectrpc-0.9.0/src/envelope.rs` and `protocol.rs` — 5-byte header:
1 flag byte + 4 big-endian length bytes, `0x02` = end-stream; streaming content-type is
`application/connect+json`; the single `StreamEventsRequest` request body is itself one
envelope-framed message, not bare JSON like the unary calls below), not guessed.

```html
<!doctype html>
<meta charset="utf-8">
<title>Chat</title>
<style>
  body { font: 14px/1.5 system-ui, sans-serif; max-width: 60rem; margin: 2rem auto; padding: 0 1rem; display: flex; gap: 1.5rem; }
  .logout { position: absolute; top: 2rem; right: 1rem; }
  .logout button { background: none; border: none; text-decoration: underline; padding: 0; font: inherit; color: inherit; cursor: pointer; }
  h1 { font-size: 1.25rem; margin: 0 0 1rem; }
  main { flex: 1; min-width: 0; display: flex; flex-direction: column; }
  aside { width: 16rem; flex-shrink: 0; }
  aside h2 { font-size: 0.9rem; color: #666; margin: 0 0 .5rem; }
  .topic-row { padding: .5rem; border-radius: 4px; cursor: pointer; margin-bottom: .25rem; }
  .topic-row:hover { background: #f4f4f4; }
  .topic-row.focus { background: #eef; font-weight: 600; }
  .topic-row .status { color: #666; font-size: .8em; }
  #newTopic { width: 100%; margin-top: .5rem; padding: .5rem; cursor: pointer; }
  #transcript { flex: 1; overflow-y: auto; border: 1px solid #eee; border-radius: 4px; padding: .75rem; margin-bottom: .75rem; min-height: 20rem; }
  .msg { padding: .4rem 0; border-bottom: 1px solid #f4f4f4; }
  .msg.notification { color: #444; font-style: italic; }
  .msg .kind { color: #999; font-size: .75em; }
  form#sendForm { display: flex; gap: .5rem; }
  #content { flex: 1; padding: .5rem; font: inherit; border: 1px solid #ccc; border-radius: 4px; }
  button { padding: .5rem 1.2rem; font: inherit; cursor: pointer; }
  .error { color: #b00; }
</style>

<form class="logout" method="post" action="/logout"><button type="submit">Log out</button></form>

<main>
  <h1>Chat</h1>
  <div id="transcript"></div>
  <form id="sendForm">
    <input id="content" placeholder="Ask something…" autofocus>
    <button type="submit">Send</button>
  </form>
</main>

<aside>
  <h2>Topics</h2>
  <div id="topics"></div>
  <button id="newTopic" type="button">New topic</button>
</aside>

<script>
  const SERVICE = "/chat.v1.ChatService";
  const transcript = document.getElementById("transcript");
  const topicsEl = document.getElementById("topics");
  let focusTopicId = null;
  let topics = [];

  function element(tag, className, text) {
    const node = document.createElement(tag);
    if (className) node.className = className;
    if (text !== undefined) node.textContent = text;
    return node;
  }

  async function callRpc(method, message) {
    const res = await fetch(`${SERVICE}/${method}`, {
      method: "POST",
      headers: { "content-type": "application/json", "connect-protocol-version": "1" },
      body: JSON.stringify(message),
    });
    const body = await res.json();
    if (!res.ok) throw new Error(body.message || `${method} failed with ${res.status}`);
    return body;
  }

  // Connect streaming wire format: each message is a 5-byte header (1 flag byte, 0x02 =
  // end-stream; 4 big-endian length bytes) followed by that many bytes of JSON. The single
  // request message for a server-streaming call is itself one such envelope, not bare JSON.
  function encodeEnvelope(message) {
    const json = new TextEncoder().encode(JSON.stringify(message));
    const out = new Uint8Array(5 + json.length);
    new DataView(out.buffer).setUint32(1, json.length, false);
    out.set(json, 5);
    return out;
  }

  function concatBytes(a, b) {
    const out = new Uint8Array(a.length + b.length);
    out.set(a, 0);
    out.set(b, a.length);
    return out;
  }

  async function* streamChatEvents() {
    const res = await fetch(`${SERVICE}/StreamEvents`, {
      method: "POST",
      headers: { "content-type": "application/connect+json", "connect-protocol-version": "1" },
      body: encodeEnvelope({}),
    });
    if (!res.ok || !res.body) throw new Error(`StreamEvents failed with ${res.status}`);
    const reader = res.body.getReader();
    let buffer = new Uint8Array(0);
    while (true) {
      const { done, value } = await reader.read();
      if (done) return;
      buffer = concatBytes(buffer, value);
      while (buffer.length >= 5) {
        const flag = buffer[0];
        const length = new DataView(buffer.buffer, buffer.byteOffset + 1, 4).getUint32(0, false);
        if (buffer.length < 5 + length) break;
        const payload = buffer.slice(5, 5 + length);
        buffer = buffer.slice(5 + length);
        if (flag & 0x02) return; // end-stream envelope
        yield JSON.parse(new TextDecoder().decode(payload));
      }
    }
  }

  function progressText(payloadJson) {
    let payload;
    try { payload = JSON.parse(payloadJson); } catch { return payloadJson; }
    const [kind, body] = Object.entries(payload)[0] || ["", {}];
    if (kind === "NodeStarted") return `Running ${body.node}…`;
    if (kind === "TaskOutput" && typeof body.chunk === "string") return body.chunk;
    if (kind === "NodeCompleted") return `${body.node} done`;
    return kind;
  }

  function renderEvent(event) {
    if (event.topicId !== focusTopicId) return;
    const div = element("div", `msg ${event.kind === "notification" ? "notification" : ""}`);
    div.appendChild(element("span", "kind", `${event.kind} `));
    const text = event.kind === "notification"
      ? (JSON.parse(event.payloadJson || "{}").text || "")
      : event.kind === "topic_progress"
      ? progressText(event.payloadJson)
      : event.kind;
    div.appendChild(document.createTextNode(text));
    transcript.appendChild(div);
    transcript.scrollTop = transcript.scrollHeight;
  }

  function renderTopics() {
    topicsEl.textContent = "";
    topics.forEach((topic) => {
      const row = element("div", `topic-row ${topic.id === focusTopicId ? "focus" : ""}`);
      row.appendChild(element("div", "", topic.title));
      row.appendChild(element("div", "status", topic.status));
      row.onclick = () => setFocus(topic.id);
      topicsEl.appendChild(row);
    });
  }

  async function setFocus(topicId) {
    if (topicId === focusTopicId) return;
    await callRpc("SetFocus", { topicId });
    focusTopicId = topicId;
    transcript.textContent = "";
    renderTopics();
  }

  async function loadSession() {
    const session = await callRpc("GetSession", {});
    topics = session.topics || [];
    focusTopicId = session.focusTopicId || (topics[0] && topics[0].id) || null;
    renderTopics();
  }

  document.getElementById("sendForm").onsubmit = async (event) => {
    event.preventDefault();
    const input = document.getElementById("content");
    const content = input.value.trim();
    if (!content || !focusTopicId) return;
    input.value = "";
    try {
      await callRpc("SendTurn", { turnId: crypto.randomUUID(), content });
    } catch (err) {
      transcript.appendChild(element("div", "msg error", err.message));
    }
  };

  document.getElementById("newTopic").onclick = async () => {
    const title = prompt("What's this topic about?");
    if (!title) return;
    try {
      const created = await callRpc("CreateTopic", {
        title,
        inputJson: JSON.stringify({ question: title }),
      });
      await loadSession();
      await setFocus(created.topicId);
    } catch (err) {
      transcript.appendChild(element("div", "msg error", err.message));
    }
  };

  (async () => {
    await loadSession();
    for (;;) {
      try {
        for await (const event of streamChatEvents()) {
          if (event.kind === "topic_completed" || event.kind === "topic_failed" || event.kind === "topic_started" || event.kind === "topic_queued") {
            await loadSession();
          }
          renderEvent(event);
        }
      } catch (err) {
        transcript.appendChild(element("div", "msg error", `Stream error: ${err.message} — retrying…`));
      }
      await new Promise((resolve) => setTimeout(resolve, 2000));
    }
  })();
</script>
```

- [ ] **Step 2: Serve it**

In `services/gateway/src/app.rs`'s `routes` function, add a third `.route_service("/chat",
ServeFile::new(config.frontend_dir().join("chat.html")))` to the `protected` router, matching
`"/"` and `"/sources"`'s exact style.

- [ ] **Step 3: Verify**

Run: `export PATH="$HOME/.cargo/bin:$PATH" && cargo check -p gateway --all-features 2>&1 | tail -30`
Expected: compiles clean (this task only adds a static file + one route line).

- [ ] **Step 4: Commit**

```bash
git add services/frontend/client/chat.html services/gateway/src/app.rs
git commit -m "chat: frontend — chat.html (transcript, topic panel, notifications)"
```

---

## Task 12: Final verification — workspace-wide checks, Docker, live end-to-end scenario

**Files:** none created; this task only runs commands.

**Interfaces:** none — this is the plan's closing gate, exercising everything Tasks 1-11 built
together through the real running stack, mirroring Tool Service's own Task 15.

- [ ] **Step 1: Workspace-wide format, lint, test**

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings 2>&1 | tail -80
cargo test --workspace --all-features 2>&1 | tail -120
```
Expected: clippy clean, every crate's tests pass — `chat` now included alongside `common`,
`engine-core`, `services/engine`, `services/tool`, `services/gateway`, `services/crawler`.

- [ ] **Step 2: Full stack up**

```bash
timeout 10 docker ps
docker compose -p aiengineeringboilerplate build chat gateway
docker compose -p aiengineeringboilerplate up -d
docker compose -p aiengineeringboilerplate ps
```
Expected: every service `healthy`, 8 total (postgres/crawler/gateway/knowledge-base/llm-router/
engine/tool/chat).

- [ ] **Step 3: Live end-to-end scenario — the spec's own scenario, two topics, each calling a
  real tool through the now-complete Tool Service**

This is the test the whole plan has been building toward. No fake server anywhere — every hop is
the actual Docker container, exercising Chat → Engine → Tool Service → knowledge-base/Brave in
one shot. A `Principal` is required on every Chat RPC (`common::principal::from_metadata`) — use
a fixed test `user_id` header the same way this stack's earlier live smoke tests did, checking
`common::principal`'s exact expected header name/format before writing the `wget` calls (read
`common/src/principal.rs` first — do not guess the header name).

```bash
CHAT="docker compose -p aiengineeringboilerplate exec chat sh -c"
# 1. Create the first (root) topic — becomes focus.
$CHAT 'wget -q -O- --header="Content-Type: application/json" --header="<principal-header>: <test-user-id>" --post-data="{
  \"title\": \"Claude Code\",
  \"input_json\": \"{\\\"question\\\": \\\"Use the kb_search tool to look up: what is Claude Code?\\\"}\"
}" http://127.0.0.1:8088/chat.v1.ChatService/CreateTopic'
# 2. Create a second (root) topic — runs in parallel, does not steal focus.
$CHAT 'wget -q -O- --header="Content-Type: application/json" --header="<principal-header>: <test-user-id>" --post-data="{
  \"title\": \"Rust news\",
  \"input_json\": \"{\\\"question\\\": \\\"Use the web_search tool to look up: latest Rust release notes\\\"}\"
}" http://127.0.0.1:8088/chat.v1.ChatService/CreateTopic'
```
Poll `GetSession` (same `wget` pattern, empty `{}` body) every few seconds until both topics show
`status: "completed"` (a handful of polls — each topic makes 2 real LLM calls plus a real Tool
Service round trip, per Tool Service's own Task 15 timing). Expected: topic 1's
`result_summary` references Claude Code (grounded in the real crawled `academy.claude.com`
content, exactly as Tool Service's Task 15 verified); topic 2 either succeeds (if a real
`BRAVE_SEARCH_API_KEY` is set) or shows `status: "failed"` with an error summary (if not — Task 6
of Tool Service's own plan already established that a missing Brave key is a clean failure, not
a crash; this is expected and acceptable, not a Chat Service defect).

Confirm via psql that both topics reached a terminal status without ever exceeding
`MAX_CONCURRENT_TOPICS`:
```bash
docker compose -p aiengineeringboilerplate exec -T postgres \
  psql -U "$(grep '^POSTGRES_USER=' .env | cut -d= -f2-)" -d "$(grep '^POSTGRES_DB=' .env | cut -d= -f2-)" -c \
  "SELECT title, status, result_summary FROM chat.topics ORDER BY created_at;"
```

- [ ] **Step 4: Confirm `chat` survives a restart mid-flight**

Start a third topic, then immediately restart the `chat` container (`docker compose -p
aiengineeringboilerplate restart chat`), then poll `GetSession` again once it's healthy. Expected:
the third topic still reaches a terminal status — `recover`'s re-subscription (Task 7) picked it
back up. If it doesn't, this is a real finding to fix before considering the plan done, not
something to note-and-skip.

- [ ] **Step 5: Run the code-review skill's gates**

Per the user's standing instruction, gates run once at the end of the day's work — this is that
point for the Chat Service unit of work. Invoke the `code-review` skill against the full diff
since this plan's own Task 1 starting commit and address any findings before considering Chat
Service done. Given tonight's Tool Service experience, expect this to find real issues — budget
real time for a fix round, not zero.

- [ ] **Step 6: Final commit (if Step 5 produced fixes)**

```bash
git add -A
git commit -m "chat: address code-review gate findings"
```
If Step 5 found nothing, there is no Step 6 commit.
