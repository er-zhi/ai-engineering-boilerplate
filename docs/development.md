# Development

The root [README](../README.md) contains the shortest path to a running stack. This document covers the details needed while changing or diagnosing it.

## Prerequisites

- Docker with Compose v2.23 or newer.
- Rust 1.98 or newer and `cargo-nextest` for host development.
- `protoc` on `PATH` (`brew install protobuf` on macOS).
- Python 3.12 and the model assets described in the [native embedder README](../native/embedder-ane/README.md#setup).

## Development Loop

Run the native embedder in one terminal and Compose Watch in another:

```bash
cd native/embedder-ane
source .venv/bin/activate
uvicorn server:app --host 0.0.0.0 --port 8086
```

```bash
docker compose up --watch
```

Compose rebuilds a changed Rust service and recopies changed Frontend files. The embedder remains a separate host process.

## Tests and Review Gates

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo nextest run --workspace
cargo deny check
cargo machete
```

Docker must be running because database tests use testcontainers. `cargo test --workspace` also works if `cargo-nextest` is unavailable.

Run the AI review workflow only after deterministic checks pass. Its entry point and required evidence format are in [the code-review skill](../.agents/skills/code-review/SKILL.md).

## Running Rust Services on the Host

Keep PostgreSQL in Compose:

```bash
docker compose up -d postgres
```

Then run services in separate terminals with the matching credentials from `.env`:

```bash
SPIDER_MAX_SIZE_BYTES=5242880 DATABASE_URL=postgres://crawler_user:<password>@127.0.0.1:5432/app cargo run -p crawler
OPENROUTER_API_KEY=<key> DATABASE_URL=postgres://llm_router_user:<password>@127.0.0.1:5432/app cargo run -p llm-router
EMBEDDER_URL=http://127.0.0.1:8086 DATABASE_URL=postgres://knowledge_base_user:<password>@127.0.0.1:5432/app cargo run -p knowledge-base
GATEWAY_AUTH_PASSWORD=<password> DATABASE_URL=postgres://gateway_user:<password>@127.0.0.1:5432/app FRONTEND_DIST_DIR=services/frontend/client cargo run -p gateway
```

Crawler accepts `CRAWL_MAX_PAGES` when you want to override its default of 100. LLM Router also needs the `LLM_<TIER>_PRIMARY` and `LLM_<TIER>_BACKUP` values from `.env`. Internal URLs default to the host ports used by the services.

## PostgreSQL

Development PostgreSQL is published only on `127.0.0.1:5432`. Use database `app`, the `postgres` superuser, or a service role to inspect exactly what that service can access. Passwords come from `.env`.

The bootstrap configuration creates roles and schemas only when the `pgdata` volume is empty. Removing that volume deletes all development data and causes bootstrap to run again; resolve the exact Compose volume name before doing so.

SeaORM schema synchronization creates missing objects but does not safely perform destructive type changes. Reset steps needed by a particular service belong in that service's README.

## Ports

| Component | Port |
|---|---:|
| Gateway | 8080 |
| Crawler | 8081 |
| LLM Router | 8083 |
| Knowledge Base | 8084 |
| Native embedder | 8086 |
| PostgreSQL | 5432 |
