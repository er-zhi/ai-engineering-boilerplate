---
name: gate-architecture
description: Use when reviewing service code, Dockerfiles, or compose.yaml in this boilerplate, or when the user asks about service boundaries, cross-service coupling, inter-service calls, adding infrastructure or a framework, shell scripts, or how the stack and its tests are started.
---

# Microservice Code Review

Architecture gate. Each service is independently deployable — own code, container, schema, and API contract. Cross-service coupling is a defect.

Rust style and input checks belong to `gate-code-quality`; schema, roles, queries, and migrations belong to `gate-database`. Report findings using the template in [code-review](../SKILL.md).

## Boundaries

- Each capability is its own service in `services/<name>/`, with no `-service` suffix
- No infrastructure or framework without a measured need and an explicit ownership boundary
- Change belongs to one service; no business logic shared across crates
- No queries, joins, or migrations touching another service's schema
- No unowned shared mutable state; shared artifacts have one writer and read-only consumers
- Cross-service calls go through gRPC contracts, not DB or filesystem
- No importing internal modules from a sibling service
- Every long-running Rust service is a Docker container and Cargo workspace member. Frontend is a one-shot file-copy container, not a runtime service or workspace member. `native/embedder-ane` is the sanctioned host-native exception because Core ML has no Linux-container equivalent; Knowledge Base reaches it over HTTP. A new exception needs a verified platform constraint.

## Parallel Work

Services are isolated so that several people or agents can each own one and never touch the same file. The rule is checked per diff: a change to service A that edits a file outside `services/a/` needs a reason.

| Shared file | Who may edit it, and how |
|---|---|
| `common/proto/<service>/vN/<service>.proto` (snake_case: `llm_router/v1/llm_router.proto`) | The service that serves it. The directory mirrors the protobuf package required by Buf STANDARD. Additive changes only: new fields get new numbers, nothing is renumbered or removed. A new service gets its own versioned directory and file, never a block in an existing contract |
| `common/src/` | Contract types a service publishes for its callers (`llm.rs` is LLM Router's tier contract), or code two services already use; the diff names the consumers |
| `Cargo.toml` (workspace) | Add a crate to `[workspace.dependencies]` once; each service's own `Cargo.toml` references it with `workspace = true` |
| `compose.yaml` | Each service edits only its own block (including its `depends_on`) plus its bootstrap lines in `postgres-bootstrap` |
| `.env.example` | Each service adds only its own keys, grouped under a comment naming the service |
| `README.md`, `docs/architecture.md` | Keep the project overview and cross-service flow consistent; service details remain in the service README |

A service's own folder holds everything else it needs: source, entities, Dockerfile, README, tests. A branch that touches one service builds and tests alone — `cargo nextest run -p <service>` and `docker compose up <service>` are the whole loop — so branches merge without conflicts.

## Capabilities, Not Topics

This is an agent platform: the code supplies general capabilities and the model decides what to
use them for. Encoding a subject area in the code is a defect — it answers one question and
leaves every neighbouring one unanswered, and the list only ever grows.

- Tools are verbs, not subjects: `web_fetch`, `web_search`, `kb_search`. A `weather`,
  `stock_quote` or `fx_rate` tool is the same `web_fetch` with a topic glued on
- No subject in a prompt: no "for weather questions, prefer…", no per-domain source lists, no
  branch on what the user is asking about. Policy is about behaviour — call a tool before
  answering, be terse, retry a failed source — and holds whatever the question is
- No keyword matching on user text to pick a route, tool, or endpoint
- A specific need becomes a general capability: "try several sources, take the first that
  answers" is a racing, retrying fetch — not a table of weather sites
- Example inputs are examples. A request that names weather and stock prices is asking for live
  external data to work, not for those two subjects to be special-cased

## Contracts

- Stable request/response shapes at the public boundary
- Callers see what a service does, never how: no storage rows, library types, or vendor names in a contract others depend on
- Internal errors map to explicit Connect error codes at the boundary; a dependency's raw message never reaches the caller
- Outbound calls centralized, not scattered raw clients
- A timeout on every outbound call; retries only for transient failures when repeating the operation is safe
- Idempotency considered for writes other services trigger
- Breaking API changes are intentional and documented

### One Declaration, Not Two

A contract is a type, and the build is what enforces it. When the same contract is written down
twice — a declared shape in one place and the code that reads it in another — nothing keeps the two
in step, and the drift surfaces as a runtime failure in front of a user.

The rule: derive one from the other, so a mismatch cannot compile. A field name, a parameter, an
enum variant or a status string that appears in two places is a finding even when both copies
currently agree, because agreeing today is not a mechanism.

- A tool's `input_schema` and the code reading its arguments: one args type, `deny_unknown_fields`,
  schema generated from that type
- A proto enum and a service matching on its spelling: the generated enum, matched exhaustively —
  never a string literal with a `_ =>` arm
- A database column's allowed values and the code comparing against them: one enum
- A config key and its reader: one constant

A test asserting that two declarations agree is not a fix. It is a reminder to keep copying by
hand, it only runs when someone runs it, and it leaves the second copy in place. Delete the second
copy instead. Tests belong on behaviour the type system cannot state — that a misspelled argument
produces an error a model can act on, not that two spellings match.

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
| Tool named after a subject (`weather`, `stock_quote`) | One capability tool the model aims itself (`web_fetch` over the sources it picks) |
| Prompt paragraph about one topic, or a per-domain source list | Behavioural policy that holds for every question |
| `if question.contains("weather")` picking a route or endpoint | Let the model choose; give it the capability, not the branch |
| Declared schema and the code reading it written separately | Generate the schema from the args type; `deny_unknown_fields` |
| String literal matched against a proto enum's spelling | The generated enum, matched exhaustively |
| Test asserting two declarations still agree | Delete one declaration; derive it from the other |
