---
name: gate-database
description: Use when creating tables, entity fields, or migrations in this boilerplate, or when the user mentions column types, database size, UUID vs bigint, data retention, cleanup, or storing logs and raw files.
---

# Database Design

Smallest correct type per column. Size grows with row count, so a wasteful type multiplies fast.

Entity field types must match the chosen Postgres types — entities drive dev schema sync, so a wrong entity type produces a wrong table. Report findings using the template in [code-review](../SKILL.md).

## Column Types

| Data | Use | Not |
|---|---|---|
| Counter, ordinal, enum discriminant | `smallint` (2B) | `bigint` by default |
| Most IDs and counts | `integer` (4B) | `bigint` under 2B values |
| Internal primary key | `bigserial` (8B) | `uuid` without a reason |
| Public or distributed ID | `uuid` (16B) | `varchar(36)` holding a UUID |
| Text with a known max | `varchar(n)` — slug 64, email 320, title 512 | unbounded `text` |
| Long content | `text` | `varchar(10000)` |
| Status, category | Postgres `enum` or `smallint` | `varchar(50)` free text |
| Timestamps | `timestamptz` | `timestamp` |
| Money, exact decimals | `numeric(p,s)` | `float`, `double` |
| Queryable fields | typed columns | one `jsonb` blob |

`text` and `varchar` store identically in Postgres — use `varchar(n)` as a validation boundary. `NOT NULL` unless the field is genuinely optional. Index only what you actually query; every index costs writes and disk.

## Storage Rules

| Data | Rule |
|---|---|
| Logs | Never in the DB — stdout/stderr → Docker logs |
| Raw HTML, files, blobs | Never permanent — store the extracted result |
| Raw data, temporary | Only with `expires_at` + automatic cleanup |
| Domain data | Permanent — entities, metadata, embeddings, status |

Prefer in-memory processing. If temporary rows are unavoidable: `expires_at timestamptz NOT NULL`, a per-table TTL, and `DELETE WHERE expires_at < now()` on startup plus a periodic interval. No temporary table without a cleanup path.

## Queries

ORM only. Service and test code reads and writes through SeaORM entities and the query builder (`Entity::find`, `insert`, `update`, `on_conflict`) and never through hand-written SQL. Raw SQL bypasses the entity types the schema is synced from, so a renamed column or changed type stops failing at compile time.

| Found in code | Use instead |
|---|---|
| `Statement::from_string`, `query_*_raw`, `execute_raw`, `execute_unprepared`, `sqlx::query` | The entity or query-builder equivalent |
| SQL assembled with `format!` | The query builder, with bound values |
| Tests reading `information_schema` or `pg_catalog` | Assert the behavior the schema guarantees through the ORM: a too-long value or a duplicate key is rejected |

The only exception is database bootstrap the ORM can't express (roles, schemas, extensions), and it lives only in `infra/postgres/`. Tests get the same bootstrap by running that script, not by repeating its SQL.

## Schema Isolation

Each service uses only its own schema, enforced by Postgres roles rather than discipline. Setup SQL and verification: [schema-isolation.md](schema-isolation.md).

Cross-service data goes over gRPC — never cross-schema SQL. Cache tables live in the service's own schema.

## Example

```sql
CREATE TABLE crawler.pages (
    id           bigserial PRIMARY KEY,
    url          varchar(2048) NOT NULL,
    title        varchar(512),
    page_type    smallint NOT NULL,
    content_hash char(64) NOT NULL,
    crawled_at   timestamptz NOT NULL
);
```

Oversized version to avoid: `uuid` PK, `text` for url/title/hash, `varchar(50)` page_type, `timestamp` without zone.
