---
name: gate-architecture
description: Use when reviewing service code, Dockerfiles, docker-compose, or migrations in this boilerplate, or when the user asks about service boundaries, schema isolation, cross-service coupling, or inter-service calls.
---

# Microservice Code Review

Architecture gate. Each service is independently deployable — own code, container, schema, and API contract. Cross-service coupling is a defect.

Rust style belongs to `gate-code-quality`; column types belong to `gate-database`. Report findings using the template in [code-review](../SKILL.md).

## Boundaries

- Change belongs to one service; no business logic shared across crates
- No queries, joins, or migrations touching another service's schema
- No shared mutable state (files, caches needing sync)
- Cross-service calls go through gRPC contracts, not DB or filesystem
- No importing internal modules from a sibling service

## Contracts

- Stable request/response shapes at the public boundary
- Outbound calls centralized, not scattered raw clients
- Timeouts and retries on every outbound call
- Idempotency considered for writes other services trigger
- Breaking API changes are intentional and documented

## Data Layer

- Service connects with its own DB role — never superuser or a shared role
- Role granted only on its own schema; `search_path` locked to it
- Tables and migrations live in that schema, never `public`
- Schema-qualified table names, own connection pool
- Prod migrations reversible or with a documented rollback

## Docker & Compose

- Own multi-stage Dockerfile, non-root user, minimal final image
- Compose entry has correct build context, env, `depends_on`, healthcheck
- Secrets via env or Compose secrets, never baked into layers
- Internal services not published to the host
- Health endpoint wired into the Docker healthcheck

## Runtime Concerns

- Structured logging with service name and request-ID correlation
- `/health` implemented; errors logged with context but no secrets
- Input validated at the API boundary before business logic
- Parameterized SQL only
- Tests don't require sibling services running — mock or testcontainers

## Common Violations

| Symptom | Fix |
|---|---|
| Service joins another schema's table | Call that service over gRPC, merge in app layer |
| Shared crate holding queries | Each service owns its repository layer |
| One Dockerfile for all services | Split per service |
| Hardcoded `localhost:5432` | `DATABASE_URL` with the Compose hostname |
| Missing timeout on outbound call | Add timeout + retry with backoff |
