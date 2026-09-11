# Schema Isolation Setup

One Postgres instance, one schema and role per service. A role cannot reach another schema.

| Service | Schema | Role |
|---|---|---|
| crawler | `crawler` | `crawler_user` |
| gateway | `gateway` | `gateway_user` |
| llm-router | `llm_router` | `llm_router_user` |

## Bootstrap

Per service, substituting the schema and role names:

```sql
CREATE SCHEMA crawler;
CREATE ROLE crawler_user LOGIN PASSWORD '${CRAWLER_DB_PASSWORD}';

GRANT USAGE ON SCHEMA crawler TO crawler_user;
GRANT ALL ON ALL TABLES IN SCHEMA crawler TO crawler_user;
GRANT ALL ON ALL SEQUENCES IN SCHEMA crawler TO crawler_user;
ALTER DEFAULT PRIVILEGES IN SCHEMA crawler GRANT ALL ON TABLES TO crawler_user;
ALTER ROLE crawler_user SET search_path TO crawler;
```

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
