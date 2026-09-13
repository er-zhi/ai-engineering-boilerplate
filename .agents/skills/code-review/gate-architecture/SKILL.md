---
name: gate-architecture
description: Use when reviewing service code, Dockerfiles, or compose.yaml in this boilerplate, or when the user asks about service boundaries, cross-service coupling, inter-service calls, adding infrastructure or a framework, shell scripts, or how the stack and its tests are started.
---

# Microservice Code Review

Architecture gate. Each service is independently deployable — own code, container, schema, and API contract. Cross-service coupling is a defect.

Rust style and input checks belong to `gate-code-quality`; schema, roles, queries, and migrations belong to `gate-database`. Report findings using the template in [code-review](../SKILL.md).

## Boundaries

- Each capability is its own service in `services/<name>/`, with no `-service` suffix
- Nothing the README's [Stack](../../../../README.md#stack) section rules out under *Not now*
- Change belongs to one service; no business logic shared across crates
- No queries, joins, or migrations touching another service's schema
- No shared mutable state (files, caches needing sync)
- Cross-service calls go through gRPC contracts, not DB or filesystem
- No importing internal modules from a sibling service
- Every service is a Docker container in `[workspace] members`, with one sanctioned exception: `native/embedder-ane` (the Apple Neural Engine via Core ML has no Linux/container equivalent, and everyone on this project develops on Apple Silicon). It is a Python process outside Docker and the Cargo workspace, reached over plain loopback HTTP, not gRPC — see [knowledge-base's README](../../../../services/knowledge-base/README.md#embedding-native-apple-silicon-only). Adding a second exception like this needs the same bar: a real, verified platform gap, not convenience.

## Parallel Work

Services are isolated so that several people or agents can each own one and never touch the same file. The rule is checked per diff: a change to service A that edits a file outside `services/a/` needs a reason.

| Shared file | Who may edit it, and how |
|---|---|
| `common/proto/<service>/vN/<service>.proto` (snake_case: `llm_router/v1/llm_router.proto`) | The service that serves it. The directory mirrors the protobuf package required by Buf STANDARD. Additive changes only: new fields get new numbers, nothing is renumbered or removed. A new service gets its own versioned directory and file, never a block in an existing contract |
| `common/src/` | Contract types a service publishes for its callers (`llm.rs` is LLM Router's tier contract), or code two services already use; the diff names the consumers |
| `Cargo.toml` (workspace) | Add a crate to `[workspace.dependencies]` once; each service's own `Cargo.toml` references it with `workspace = true` |
| `compose.yaml` | Each service edits only its own block (including its `depends_on`) plus its bootstrap lines in `postgres-bootstrap` |
| `.env.example` | Each service adds only its own keys, grouped under a comment naming the service |
| `README.md` | The rows and sections that name the service: Status, Services, the diagram, Getting Started; no prose elsewhere |

A service's own folder holds everything else it needs: source, entities, Dockerfile, README, tests. A branch that touches one service builds and tests alone — `cargo nextest run -p <service>` and `docker compose up <service>` are the whole loop — so branches merge without conflicts.

## Contracts

- Stable request/response shapes at the public boundary
- Callers see what a service does, never how: no storage rows, library types, or vendor names in a contract others depend on
- Internal errors map to the shared codes in `common/errors/` at the boundary; a dependency's message never reaches the caller
- Outbound calls centralized, not scattered raw clients
- A timeout, and retries with backoff, on every outbound call
- Idempotency considered for writes other services trigger
- Breaking API changes are intentional and documented

## Docker & Compose

- Own multi-stage Dockerfile, non-root user, minimal final image
- Compose entry has correct build context, env, `depends_on`, and a healthcheck wired to the service's `/health`
- Secrets via env or Compose secrets, never baked into layers
- Internal services not published to the host
- **No shell scripts.** Everything runs through the tools' own documented commands: `docker compose up` for the stack, `docker compose run --rm <service> <command>` for a one-off command in a service's container, `cargo nextest run` for tests. Setup that needs steps goes in a Dockerfile stage, a Compose service, or SQL that the official Postgres image runs from `/docker-entrypoint-initdb.d` — never in a `.sh` file. This is the Docker case of [the documented-way rule](../gate-facts/SKILL.md#documented-way-not-a-workaround).

## Runtime Concerns

- Structured logging with service name and request-ID correlation
- Errors logged with context but no secrets
- Tests don't require sibling services running — mock or testcontainers

## Common Violations

| Symptom | Fix |
|---|---|
| Service joins another schema's table | Call that service over gRPC, merge in app layer |
| Shared crate holding queries | Each service owns its repository layer |
| One Dockerfile for all services | Split per service |
| Hardcoded `localhost:5432` | `DATABASE_URL` with the Compose hostname |
| Proto field that exists only because a table has that column | Send what the caller needs; convert in the service's mapper |
| A dependency's type or error in a shared signature (`sea_orm::DbErr`, `spider::Page`) | Own type at the boundary, converted where it crosses |
| Caller branching on the backend behind a trait | One API; the trait picks the backend |
| Shell script wrapping `docker` or `cargo` | The tool's documented command, or a Compose service |
