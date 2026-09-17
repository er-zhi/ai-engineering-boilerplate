# Common

Shared Rust contracts and utilities. Services may depend on `common`; they must not import another service's internal modules.

## Contents

- `proto/` — versioned protobuf contracts for Crawler, Knowledge Base, LLM Router, Engine, Tool, and Chat.
- `src/extract.rs` — readable title, main text, and content hash from fetched HTML. Two consumers: Crawler, and Tool Service's `web_fetch`.
- `src/principal.rs` — the caller identity headers Gateway stamps on proxied requests. `session_id` is carried (which is why `Principal` is not `Copy`) even though nothing reads it back out yet: it is the value Gateway's logging interceptor will correlate on.
- `src/logging.rs` — structured tracing initialization.
- `src/test_db.rs` — testcontainers support, enabled by the `test-support` feature.
- `src/lib.rs` — generated protocol modules and public exports.

## Placement Rules

- A value crossing a service boundary belongs in that service's versioned proto contract.
- Code belongs here only after at least two services need the same behavior or when it is part of a published contract.
- Database entities, queries, mappers, and domain behavior remain inside the owning service.
- Generated proto types are canonical across services; entity types are canonical inside a service. Do not create hand-written mirrors.
- Proto changes are additive within a version: add new field numbers, never reuse or renumber existing ones.
- A module with one consumer moves home to that service: quality-tier limits live in `llm-router`, the session `CacheStore` in `gateway`.
