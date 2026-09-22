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

Docker must be running for database tests. Nextest starts one disposable Postgres fixture for the suite, while every test gets its own database so the tests remain isolated and parallel; the fixture is removed when nextest exits. `cargo test --workspace` also works if `cargo-nextest` is unavailable and falls back to a testcontainer per database test.

The whole suite runs in well under a minute. Two settings keep it there, and both fail silently when
they drift:

- A service whose tests take a database must appear in **both** the setup-script filter in
  `.config/nextest.toml` and the `SCHEMAS` list in `.config/nextest-postgres.sh`. A service missing
  from either falls back to starting its own Postgres container per test, which costs seconds per
  test rather than milliseconds.
- A service whose unit tests need `test_db` must depend on itself in its `[dev-dependencies]`
  (`chat = { path = ".", features = ["test-support"] }`). Without that the tests are not compiled at
  all by a plain `cargo nextest run --workspace` — they are skipped, not reported.

## Golden Cases

The deterministic suite proves the code does what it was written to do. It cannot prove the product
answers. That is what `golden/` is for: end-to-end cases run against a live stack through Gateway —
the same door a browser uses — covering the declarative tools, multi-theme splitting and the
knowledge base.

```bash
set -a && source .env && set +a
cargo run -p golden > /tmp/golden-run.json
```

The runner judges nothing. Answers are generated text, so it records what happened — how many
topics a message opened, what each was asked, what came back, when each finished — and a model
reads the run against each case's written expectation. The procedure, including the provider check
that has to come first, is in [the golden skill](../.agents/skills/golden/SKILL.md).

These are not part of `cargo nextest run --workspace`: they need the stack up, real API keys and a
judgement no assertion makes.

## Image Build Speed

`rust-toolchain.toml`'s channel and the `FROM rust:<version>-alpine` tag in every
`services/*/Dockerfile` must name the **same exact patch version**. A floating tag such as
`rust:1.98-alpine3.24` drifts to the newest patch release, and rustup then downloads the whole
pinned toolchain inside every image build — about 48 seconds before cargo even starts. When the
toolchain is bumped, bump both.

Run the AI review workflow only after deterministic checks pass. Its entry point and required evidence format are in [the code-review skill](../.agents/skills/code-review/SKILL.md).

## Build Footprint

`target/` grows without bound if nothing is done about it: Cargo never removes the artefacts of a
dependency version that has moved on, and every rebuild adds more. A long session of rebuilds grew
it to **129 GB across 458 000 files** and filled the disk, at which point nothing could run — not
the tests, not Docker, not even a shell command, because the shell could no longer write its own
output.

Two things keep it down. `[profile.dev]` and `[profile.test]` in the root `Cargo.toml` set
`debug = "line-tables-only"`, which keeps the file and line a panic backtrace prints and drops the
variable and type tables only a debugger reads; a full workspace build is **3 GB** rather than 93.
And `cargo clean` remains the answer when it creeps up anyway — it costs one full rebuild, about
three minutes here.

If you attach a debugger, put `debug = true` in a profile of your own rather than changing these.

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
