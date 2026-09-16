---
name: gate-database
description: Use when creating tables, entity fields, queries, or migrations in this boilerplate, or when the user mentions column types, database size, UUID vs bigint, raw SQL, schema isolation, database roles, data retention, cleanup, or storing logs and raw files.
---

# Database Design

Smallest correct type per column. Size grows with row count, so a wasteful type multiplies fast.

Entities are the source of truth: schema sync builds the tables from them on startup in dev, and prod gets migration files, each reversible or shipped with a documented rollback. Entity field types must match the chosen Postgres types, so a wrong entity type produces a wrong table. Report findings using the template in [code-review](../SKILL.md).

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
| Opaque/semi-structured payload (state, checkpoints, event payloads, metadata) | `jsonb` (SeaORM `column_type = "JsonBinary"`) | plain `json` (SeaORM `"Json"`) — text storage with no indexing, no operators, and no reason to prefer it over binary once the field is a blob at all |

`text` and `varchar` store identically in Postgres; `varchar(n)` makes the length a schema guarantee. The code enforces the same limit where the value enters ([gate-code-quality](../gate-code-quality/SKILL.md#boundaries-and-limits)), so the database never has to reject it. `NOT NULL` unless the field is genuinely optional. Index only what you actually query; every index costs writes and disk.

## Storage Rules

| Data | Rule |
|---|---|
| Operational logs | Never in the DB — stdout/stderr → Docker logs |
| Audit or usage records | Domain tables with an explicit purpose, bounded payload retention, and cleanup |
| Raw HTML, files, blobs | Never permanent — store the extracted result |
| Raw data, temporary | Only with `expires_at` + automatic cleanup |
| Domain data | Permanent — entities, metadata, embeddings, status |

Prefer in-memory processing. If temporary rows are unavoidable: `expires_at timestamptz NOT NULL`, a per-table TTL, and `DELETE WHERE expires_at < now()` on startup plus a periodic interval. No temporary table without a cleanup path.

## Queries

ORM only. Service and test code reads and writes through SeaORM entities and the query builder (`Entity::find`, `insert`, `update`, `on_conflict`) and never through hand-written SQL. Raw SQL bypasses the entity types the schema is synced from, so a renamed column or changed type stops failing at compile time.

| Found in code | Use instead |
|---|---|
| `Statement::from_string`, `query_*_raw`, `execute_raw`, `execute_unprepared`, `sqlx::query` reading or writing rows | The entity or query-builder equivalent |
| SQL assembled with `format!` | The query builder, with bound values |
| Tests reading `information_schema` or `pg_catalog` | Assert the behavior the schema guarantees through the ORM: a too-long value or a duplicate key is rejected |

The only exceptions are schema objects the ORM genuinely cannot express:

- Database bootstrap (roles, schemas, extensions) — lives only in the `postgres-bootstrap` config in `compose.yaml`, run once by Postgres itself before any service ever connects. A test container that needs a service role creates it with the same statements.
- A vendor-specific index type entity-driven schema-sync has no derive attribute for (pgvector `USING hnsw`, a `GIN` index on `tsvector`/an array column) — this can't live in bootstrap, because bootstrap runs before schema-sync has created the table. It runs once at the owning service's own startup, immediately after schema-sync: `ConnectionTrait::execute_unprepared` naming one literal `CREATE INDEX IF NOT EXISTS ...` string is the sanctioned mechanism for exactly this, and only this — never a query, never touching rows, never string-built from anything but a compile-time literal. Document each one where its entity is defined.

Neither exception licenses raw SQL for anything else — a query that reads or writes rows still goes through the entity or the query builder, `Expr::cust`/`Expr::cust_with_values` included for the one operator or function the builder has no named method for (pgvector `<=>`, `ts_rank`, array `&&`) inside an otherwise-typed `Select` chain, never a whole hand-assembled statement.

## Schema Isolation

Each service uses only its own schema, enforced by Postgres roles rather than discipline. Bootstrap and verification: [schema-isolation.md](schema-isolation.md).

- The service connects as its own role — never the superuser or a shared role.
- Every entity sets `schema_name` to the service's schema; nothing lives in `public`.

## Example

```sql
CREATE TABLE crawler.pages (
    id           bigserial PRIMARY KEY,
    url          varchar(2048) NOT NULL,
    title        varchar(512) NOT NULL,
    main_text    text NOT NULL,
    content_hash char(64) NOT NULL,
    http_status  smallint NOT NULL,
    crawled_at   timestamptz NOT NULL
);
```

Oversized version to avoid: `uuid` PK, `text` for URL/title/hash, `integer` for an HTTP status, and `timestamp` without a time zone.
