# Common

Shared Rust contracts and utilities. Services may depend on `common`; they must not import another service's internal modules.

## Contents

- `proto/` — versioned protobuf contracts for Crawler, Knowledge Base, and LLM Router.
- `src/cache/` — the `CacheStore` abstraction and PostgreSQL implementation currently used for Gateway sessions.
- `src/llm.rs` — quality-tier limits, sampling settings, and request validation shared with LLM Router callers.
- `src/logging.rs` — structured tracing initialization.
- `src/test_db.rs` — testcontainers support, enabled by the `test-support` feature.
- `src/lib.rs` — generated protocol modules and public exports.

## Placement Rules

- A value crossing a service boundary belongs in that service's versioned proto contract.
- Code belongs here only after at least two services need the same behavior or when it is part of a published contract.
- Database entities, queries, mappers, and domain behavior remain inside the owning service.
- Generated proto types are canonical across services; entity types are canonical inside a service. Do not create hand-written mirrors.
- Proto changes are additive within a version: add new field numbers, never reuse or renumber existing ones.

`CacheStore` is backend-neutral, but PostgreSQL is the only implementation and there is no runtime `CACHE_BACKEND` switch. A service using it still stores rows in its own schema.
