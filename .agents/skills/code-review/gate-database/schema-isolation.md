# Schema Isolation Setup

One Postgres instance, one schema and role per service. A role cannot reach another schema.

| Service | Schema | Role |
|---|---|---|
| crawler | `crawler` | `crawler_user` |
| gateway | `gateway` | `gateway_user` |
| llm-router | `llm_router` | `llm_router_user` |

## Bootstrap

[`infra/postgres/init.sh`](../../../../infra/postgres/init.sh) runs on Postgres's first start and does this per service:

```sql
CREATE ROLE crawler_user LOGIN PASSWORD '<CRAWLER_DB_PASSWORD>';
CREATE SCHEMA crawler AUTHORIZATION crawler_user;
ALTER ROLE crawler_user SET search_path TO crawler, public;
```

The role owns its schema, so it can create and alter its own tables and has no rights in any other schema. `public` stays on the search path only so the pgvector type resolves.

Each service's `.env` uses its own credentials:

```env
DATABASE_URL=postgres://crawler_user:${CRAWLER_DB_PASSWORD}@postgres:5432/app
```

Dev-mode entity sync runs as the service role, so it can only create or alter tables in that schema.

## Verify

```sql
SET ROLE crawler_user;
SELECT * FROM gateway.crawl_jobs;  -- must fail: permission denied
SELECT * FROM crawler.pages;       -- must succeed
```
