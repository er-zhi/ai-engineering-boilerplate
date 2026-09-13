# AI Engineering Boilerplate

A Rust microservice boilerplate for AI products, built to go from code change to running service in seconds on a local machine. Prototyping now, scaling later: the code holds up in production, while infrastructure stays lean (one Postgres, Docker Compose, no Kafka or Kubernetes) until the product proves itself. Service boundaries are clean enough to move to the cloud later without a rewrite.

It ships with a crawler that finds and denoises pages, a knowledge-base service that enriches and embeds what changed, an LLM router that picks models by quality tier, and a set of code-review skills that AI agents run in parallel.

**Requires an Apple Silicon Mac (M1 or newer).** Embedding runs on a native process outside Docker, on the chip's Neural Engine — see [Running the Native Embedder](#running-the-native-embedder) — because there is no Linux-container equivalent of Apple's Core ML or Metal. Everything else in the stack is plain Docker Compose and would run anywhere; this one piece is the deliberate exception.

## Status

Early stage. Most of the system is specified but not yet built.

| Part | State |
|---|---|
| Cargo workspace and `common` crate, which generates Connect/gRPC stubs from `common/proto/` | Done |
| `frontend` service, which owns the web UI (`services/frontend/`) | Done |
| Code-review skills (`.agents/skills/code-review/`) | Done |
| `crawler` and `gateway` services, end to end over Connect + gRPC | Done — jobs persisted in `crawler.crawl_jobs`, idempotent `StartCrawl` |
| Docker Compose with hot reload | Done |
| Crawling with spider-rs: scope rules, page cap, live progress | Done |
| Postgres 18 + pgvector, one schema and role per service | Done |
| Crawler page storage: title, main text, and content hash in `crawler.pages` | Done |
| `llm-router` service: tier contract, fallback, request log | Done |
| `knowledge-base` service: `Ingest`, own dedup, LLM enrichment, embedding via the native process | Done — crawler hands changed pages to it |
| `native/embedder-ane`: Qwen3-Embedding-0.6B on the Neural Engine via Core ML, 8-bit, ~20 ms a call | Done — Apple Silicon only, run outside Docker |
| `CacheStore` trait + Postgres impl, Gateway session auth (login/logout, `gateway.sessions`) | Done |
| Frontend build-once-and-exit, Gateway serves its output directly, `no-store` on every page | Done |
| Retrieval for agents: passages with contextual headers, hybrid search (HNSW + GIN) with Reciprocal Rank Fusion, page-type filter | Done — no reranker or BM25 yet, see the design spec |

## Architecture

```mermaid
flowchart LR
  browser["Browser"] -->|"page load + Connect JSON"| gateway
  gateway -->|HTTP| frontend
  gateway -->|gRPC| crawler
  crawler -->|"gRPC, changed pages only"| kb["knowledge-base"]
  kb -->|gRPC| llm["llm-router"]
  llm -->|HTTPS| openrouter[("OpenRouter")]
  kb -->|"HTTP, loopback only"| embedder["embedder-ane<br/>(native, Apple Silicon, Neural Engine)"]
  crawler --- pg[("PostgreSQL<br/>schema per service + pgvector")]
  kb --- pg
  llm --- pg
```

- **Gateway** is the only service reachable from outside. It routes requests and holds no business logic; each service validates what it receives at its own API boundary.
- The browser loads the page and calls RPCs from the same origin, so there is no CORS anywhere in the system. Gateway proxies page routes to Frontend and answers every Connect path itself.
- Services talk to each other over gRPC only. Shared contracts live in `common/proto/`.
- Each service that stores data connects to Postgres with its own role, and that role can only access the service's own schema. Postgres enforces the isolation — see [schema-isolation.md](.agents/skills/code-review/gate-database/schema-isolation.md).
- Crawler and knowledge-base each do their own, independent content-hash dedup — crawler decides whether to call knowledge-base at all; knowledge-base never trusts that decision and hashes again before spending anything on an LLM or embedding call.
- Every page and RPC behind Gateway requires a session, checked against `gateway.sessions` on every call — see [Gateway's Authentication section](services/gateway/README.md#authentication).
- `native/` is the one sanctioned exception to "every service is a container": Core ML and Metal have no Linux backend, so the embedder runs directly on the host and knowledge-base reaches it over loopback HTTP instead of gRPC. See [Running the Native Embedder](#running-the-native-embedder) and [knowledge-base's README](services/knowledge-base/README.md#embedding-native-apple-silicon-only).

## Stack

| Use | Tool |
|---|---|
| Language | Rust (edition 2024, 1.98+) |
| Inter-service | gRPC via [connectrpc](https://crates.io/crates/connectrpc) and [buffa](https://crates.io/crates/buffa), contracts in `common/proto/` |
| External API | Connect via Gateway ([axum](https://crates.io/crates/axum)) — proto-defined RPC callable from plain `fetch`, no REST |
| Database | PostgreSQL 18, one instance, schema per service, pgvector, through [SeaORM](https://crates.io/crates/sea-orm) entities |
| Cache | PostgreSQL behind the `CacheStore` trait in `common/cache/`, swappable for Redis later |
| LLM | [OpenRouter](https://openrouter.ai/), reached through the LLM Router |
| Embedding | [Qwen3-Embedding-0.6B](https://huggingface.co/Qwen/Qwen3-Embedding-0.6B) on the Neural Engine via Core ML (8-bit), native on the host — never in Docker |
| Runtime | Docker Compose, one Dockerfile per service, Alpine base with static musl binaries; `native/embedder-ane` is the one non-Dockerized process |

**Not now:** Redis, Kafka, Kubernetes, cloud tooling, a second database, heavy frameworks. Need a cache? Postgres behind `CacheStore`. Cloud scaling comes later, on top of this codebase — not inside it.

## Layout

```
.
├── Cargo.toml                    # workspace
├── compose.yaml                  # the whole stack: images, resource limits, Postgres bootstrap
├── common/                       # shared crate: proto contracts + generated stubs
├── services/
│   ├── crawler/                  # crawl, denoise, dedup, hand off changed pages
│   ├── frontend/                 # web UI: pages, styles, client-side logic
│   ├── gateway/                  # public Connect API, proxies page routes to frontend
│   ├── knowledge-base/           # store, enrich, embed, search
│   └── llm-router/               # LLM calls by quality tier, with fallback
├── native/
│   └── embedder-ane/             # Qwen3 on the Neural Engine via Core ML — Apple Silicon only, not Dockerized
└── .agents/skills/code-review/   # review gates for AI agents
```

## Services

| Service | What it does |
|---|---|
| [crawler](services/crawler/README.md) | Crawls sites with [spider-rs](https://github.com/spider-rs/spider), strips each page down to its main content, and keeps its own copy for dedup. A page whose content actually changed is handed to knowledge-base; unchanged pages never leave crawler. |
| [knowledge-base](services/knowledge-base/README.md) | The canonical, source-agnostic store: hashes content independently, calls llm-router for page type/keywords/summary, splits content into passages embedded via the native `embedder-ane` process, and writes it all in one transaction. `Search` is hybrid retrieval over passages fused by reciprocal rank, for agents to ground their own answers. |
| [frontend](services/frontend/README.md) | Owns the entire web UI — markup, styles, and all client-side logic. Serves pages on the internal network only; the browser reaches them through Gateway. |
| [gateway](services/gateway/README.md) | The only service exposed to the host. Re-exposes `CrawlerService` over Connect so browsers can call it with plain `fetch`, forwards to internal services over gRPC, and proxies page routes to Frontend. |
| [llm-router](services/llm-router/README.md) | Callers ask for a `low`, `medium`, or `high` tier instead of a model name. The router maps each tier to a primary and backup model on OpenRouter. |

Shared code rules are in [common/README.md](common/README.md).

## Rules

Each rule is enforced by a code-review gate, which holds the details.

- **Structured logs** — every service writes one JSON line per event to stderr through `tracing`, with the crate name as `target`; nothing goes to stdout.
- **Self-documenting code, no comments** — names carry the meaning; a one-line summary at the top of a file is the only comment. [gate-code-quality](.agents/skills/code-review/gate-code-quality/SKILL.md)
- **Check input once, at the entry** — every outside value is validated where it enters and every size has a named limit, so the code behind the entry stays thin. [gate-code-quality](.agents/skills/code-review/gate-code-quality/SKILL.md#boundaries-and-limits)
- **Fix bugs at their origin** — never with a new condition for the one reported case. [gate-bug-fix](.agents/skills/code-review/gate-bug-fix/SKILL.md)
- **One service per capability**, in `services/<name>/`, talking to other services over gRPC only. [gate-architecture](.agents/skills/code-review/gate-architecture/SKILL.md)
- **Callers see what, not how** — a contract exposes the capability, never the storage layout, library type, or vendor behind it. [gate-architecture](.agents/skills/code-review/gate-architecture/SKILL.md#contracts)
- **No workarounds** — every tool is used the way its official docs describe, so no shell scripts around `docker compose` or `cargo`. [gate-facts](.agents/skills/code-review/gate-facts/SKILL.md#documented-way-not-a-workaround)
- **Entities drive the schema** — sync on startup in dev, migration files for prod. Schema isolation enforced by Postgres roles, the smallest correct column type, ORM only, no logs or permanent raw HTML in the database. [gate-database](.agents/skills/code-review/gate-database/SKILL.md)
- **Tests are meaningful, fast, and reliable** — parallel, no network in unit tests, the whole suite under 2 minutes. [gate-testing](.agents/skills/code-review/gate-testing/SKILL.md)

## Working in Parallel

The layout exists so that several people or AI agents can each own one service at a time without merge conflicts. One task touches one `services/<name>/` folder; the shared files (`common/proto/`, workspace `Cargo.toml`, `compose.yaml`, `.env.example`, this README) each have an ownership rule in [gate-architecture](.agents/skills/code-review/gate-architecture/SKILL.md#parallel-work). A service builds, tests, and runs alone:

```bash
cargo nextest run -p crawler       # this service's tests only
docker compose up crawler          # this service plus what it depends on
```

Proto contracts change additively, so a service can ship a new field before any caller reads it, and callers upgrade on their own schedule.

## Code Review Skills

Agent skills live in `.agents/skills/`, grouped by purpose, and work in both Claude Code and Codex. Code review is the first group: a review runs its gates in parallel, one agent per gate, and each gate reports how long it took so the slowest one can be optimized. Start with [SKILL.md](.agents/skills/code-review/SKILL.md), which lists every gate.

## Getting Started

Running the stack needs only Docker with Compose v2.23 or newer, the version that reads the Postgres bootstrap `compose.yaml` carries inline. Nothing runs through a wrapper script: every command below is the tool's own, as its documentation describes.

```bash
git clone https://github.com/er-zhi/ai-engineering-boilerplate.git
cd ai-engineering-boilerplate
cp .env.example .env   # then replace the change-me passwords (openssl rand -hex 24), set OPENROUTER_API_KEY and GATEWAY_AUTH_PASSWORD
docker compose up
```

Compose refuses to start while `OPENROUTER_API_KEY` or `GATEWAY_AUTH_PASSWORD` is unset, and LLM Router exits at startup if the key is not valid. Then open <http://localhost:8080> and log in with `GATEWAY_AUTH_PASSWORD`. Only Gateway is published to the host; Crawler, Frontend, LLM Router, and knowledge-base stay on the internal network, and Postgres is published on `127.0.0.1` alone.

While developing, start the same stack with [Compose Watch](https://docs.docker.com/compose/how-tos/file-watch/):

```bash
docker compose up --watch
```

Saving a file rebuilds that service's image and replaces its container. Development and production run the very same image, under the resource limits `compose.yaml` sets for every service.

## Running the Native Embedder

Everyone on this project develops on Apple Silicon (M1 or newer), so embedding is not Dockerized: `native/embedder-ane` runs [Qwen3-Embedding-0.6B](https://huggingface.co/Qwen/Qwen3-Embedding-0.6B) directly on the Mac's Neural Engine through Core ML, and `knowledge-base` (in Docker) calls it over `http://host.docker.internal:8086`. Without it running, `Ingest` and `Search` fail with "could not reach the native embedder" — everything else in the stack works fine.

**One-time setup** — a Python 3.12 virtualenv plus the model files (about 1.1 GB); the exact steps, including how the 8-bit model files are produced, are in [native/embedder-ane/README.md](native/embedder-ane/README.md#setup).

**Every time you develop**, start it in its own terminal before (or any time before) you need `Ingest` or `Search` to work — it is a plain long-running process, not a Compose service, so `docker compose up` does not start or stop it:

```bash
cd native/embedder-ane
source .venv/bin/activate
uvicorn server:app --host 0.0.0.0 --port 8086
```

It is silent for 30–60 s while Core ML compiles the model for the Neural Engine, then prints `Application startup complete`; `GET http://localhost:8086/health` confirms it is up.

It is a Python process, not a Cargo workspace member (see [gate-architecture](.agents/skills/code-review/gate-architecture/SKILL.md#boundaries)), so `cargo build --workspace` / `cargo nextest run --workspace` never touch it and nothing here breaks on a non-Mac CI runner. Core ML is macOS-only; moving this stack to a Linux server would mean re-implementing the embedder there (the same model runs on CUDA via `candle` or PyTorch) and re-embedding every document.

### Database

In dev, Postgres listens on `127.0.0.1:5432`, never on the network. Connect any client (psql, the Database Client extension in VS Code or Cursor, DBeaver) with:

| Setting | Value |
|---|---|
| Server type | PostgreSQL |
| Host | `127.0.0.1` |
| Port | `5432` |
| Database | `app` |
| Username | `postgres` (superuser), or `crawler_user` / `llm_router_user` / `knowledge_base_user` / `gateway_user` to see exactly what one service sees |
| Password | the matching value from `.env` |

The `postgres-bootstrap` config in `compose.yaml` creates the schemas and roles only on the first start, when the `pgdata` volume is empty. To apply changed passwords, remove that volume (`docker volume ls | grep pgdata`, then `docker volume rm <name>`), which deletes all data.

### Tests

```bash
cargo nextest run --workspace
```

This is the standard [cargo-nextest](https://nexte.st/) runner; `cargo test --workspace` also works, without per-test timing. It needs Rust 1.98+ and `protoc` (see below), plus a running Docker: the storage tests start their own Postgres through testcontainers.

### On the Host

The services also run outside Docker. You need Rust 1.98+ and `protoc` on your `PATH`, which `connectrpc-build` uses to compile the proto files (`brew install protobuf` on macOS, `apt install protobuf-compiler` on Debian/Ubuntu). Keep Postgres in Compose and start each service in its own terminal:

```bash
docker compose up -d postgres
DATABASE_URL=postgres://crawler_user:<CRAWLER_DB_PASSWORD>@127.0.0.1:5432/app cargo run -p crawler
OPENROUTER_API_KEY=<key> DATABASE_URL=postgres://llm_router_user:<LLM_ROUTER_DB_PASSWORD>@127.0.0.1:5432/app cargo run -p llm-router
DATABASE_URL=postgres://knowledge_base_user:<KNOWLEDGE_BASE_DB_PASSWORD>@127.0.0.1:5432/app cargo run -p knowledge-base
GATEWAY_AUTH_PASSWORD=<password> DATABASE_URL=postgres://gateway_user:<GATEWAY_DB_PASSWORD>@127.0.0.1:5432/app FRONTEND_DIST_DIR=services/frontend/client cargo run -p gateway
```

Replace the placeholders with the values from `.env`; LLM Router also reads the `LLM_<TIER>_PRIMARY` / `LLM_<TIER>_BACKUP` variables listed there. Frontend has no server to run outside Docker — `FRONTEND_DIST_DIR` points Gateway straight at `client/` instead of the shared volume Compose creates. Gateway finds Crawler on `127.0.0.1:8081` by default; LLM Router listens on `8083`, knowledge-base on `8084`, and crawler finds knowledge-base there via `KNOWLEDGE_BASE_URL`.

### Calling the API

Calling an RPC takes no client library — the Connect protocol is a `POST` with a JSON body:

```bash
curl -X POST localhost:8080/crawler.v1.CrawlerService/StartCrawl \
  -H 'content-type: application/json' -H 'connect-protocol-version: 1' \
  -d '{"baseUrl":"https://example.com","scope":{"excludePatterns":["*/admin/*"]}}'
```
