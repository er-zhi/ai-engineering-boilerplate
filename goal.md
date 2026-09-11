# Project Goal

Product-ready Rust codebase optimized for **speed to develop and ship** on a local machine. Prototyping now, scaling later. Code quality holds up in production; infrastructure stays lean until the product proves itself.

## Stack

| Use | Tool |
|---|---|
| Language | Rust |
| Runtime | Docker Compose (one Dockerfile per service, Alpine + static musl) |
| Database | PostgreSQL — one instance, schema-per-service, pgvector |
| Cache | PostgreSQL behind `CacheStore` trait in `common/cache/` |
| Inter-service | gRPC, proto in `common/proto/` |
| External API | Connect via Gateway — proto-defined RPC, browser-callable, no REST |
| LLM | OpenRouter via LLM Router |

**Not now:** Redis, Kafka, K8s, cloud tooling, second DB, heavy frameworks. Need a cache? Postgres + wrapper. Cloud scaling comes later, on top of this codebase — not inside it.

## Dev Velocity

- **Hot reload** — source volume-mounted, `watchexec` restarts on change. Prod Dockerfile separate; dev overrides in `docker-compose.override.yml`.
- **Entity-driven schema** — entities are the source of truth, schema auto-syncs on startup in dev. Migration files only for prod.
- **One command** — `docker compose up` brings up the full stack with health checks.
- Target: code change → running service in seconds.

## Architecture

- Each capability is an independent service in `services/<name>/` (no `-service` suffix).
- Boundaries stay clean so a later move to cloud/K8s needs no rewrite.
- Cross-service data flows through gRPC only.
- Shared code lives in [common/](common/README.md) — proto, errors, cache, test helpers, utils.

## Data

- **Schema isolation is enforced** — each service connects with its own DB role granted access only to its schema. Postgres denies cross-schema access; it is not a convention.
- **Smallest correct column type** per column.
- **No logs in DB** — stdout only. **No permanent raw data** — store extracted content. Temporary raw needs `expires_at` + cleanup.

See [gate-database](.agents/skills/code-review/gate-database/SKILL.md).

## Testing

Meaningful, fast, reliable — not coverage theater.

- Unit: domain logic, mappers, pure functions
- Integration: DB, gRPC, HTTP via testcontainers
- Contract: proto compatibility between services
- Parallel, no inter-test dependencies, no network in unit tests
- Runner: `cargo nextest` — per-test timing and `SLOW` markers come free
- Full suite under 2 minutes

## Services

| Service | Role |
|---|---|
| [crawler](services/crawler/README.md) | Crawl sites, extract content, LLM enrichment, vector search; skips unchanged pages |
| [frontend](services/frontend/README.md) | Web UI — all markup, styles, and client-side logic; reached through Gateway |
| [gateway](services/gateway/README.md) | External Connect API, routes to internal services over gRPC, proxies page routes to Frontend |
| [llm-router](services/llm-router/README.md) | LLM abstraction by quality tier with fallback |

## Skills

Agent skills live in `.agents/skills/`, grouped by purpose, and are runtime-agnostic — they work in both Claude Code and Codex. Code review is one group and runs its gates in parallel, one agent per gate, each reporting its own duration so the slowest gate can be optimized — see [.agents/skills/code-review/SKILL.md](.agents/skills/code-review/SKILL.md).

| Gate | Checks |
|---|---|
| [gate-architecture](.agents/skills/code-review/gate-architecture/SKILL.md) | Boundaries, Docker, schema isolation |
| [gate-code-quality](.agents/skills/code-review/gate-code-quality/SKILL.md) | Durability, naming, minimalism |
| [gate-common](.agents/skills/code-review/gate-common/SKILL.md) | Type chain, shared code |
| [gate-database](.agents/skills/code-review/gate-database/SKILL.md) | Column types, storage rules |
| [gate-testing](.agents/skills/code-review/gate-testing/SKILL.md) | Runs the suite, reports pass/fail and slowest tests |
| [gate-facts](.agents/skills/code-review/gate-facts/SKILL.md) | External claims verified against official docs |
| [gate-evidence](.agents/skills/code-review/gate-evidence/SKILL.md) | Proof the change ran — terminal output, screenshots |
| [gate-second-opinion](.agents/skills/code-review/gate-second-opinion/SKILL.md) | Cross-model judgment — GLM plus Codex or Claude, in parallel |
