# Schema Isolation Setup

One Postgres instance, one schema and role per service. A role cannot reach another schema.

| Service | Schema | Role | Password variable |
|---|---|---|---|
| crawler | `crawler` | `crawler_user` | `CRAWLER_DB_PASSWORD` |
| gateway | `gateway` | `gateway_user` | `GATEWAY_DB_PASSWORD` |
| llm-router | `llm_router` | `llm_router_user` | `LLM_ROUTER_DB_PASSWORD` |
| knowledge-base | `knowledge_base` | `knowledge_base_user` | `KNOWLEDGE_BASE_DB_PASSWORD` |

## Bootstrap

The bootstrap is SQL that `compose.yaml` holds as a [config](https://docs.docker.com/reference/compose-file/configs/) and mounts at `/docker-entrypoint-initdb.d/init.sql`. The [official Postgres image](https://github.com/docker-library/docs/blob/master/postgres/README.md#initialization-scripts) runs `*.sql` files there as `POSTGRES_USER`, and only when the data directory is empty. No shell script is involved: psql's [`\getenv`](https://www.postgresql.org/docs/current/app-psql.html#APP-PSQL-META-COMMAND-GETENV) reads each password from the environment Compose passes to the container.

For each service in the table, the bootstrap:

1. creates the login role with the password from its variable;
2. creates the schema, owned by that role;
3. sets the role's `search_path` to `<schema>, public`.

It also enables the `vector` extension once. The role owns its schema, so it can create and alter its own tables and has no rights in any other schema. `public` stays on the search path only so the pgvector type resolves.

Compose builds each storing service's `DATABASE_URL` from its role and the matching password in `.env` — see the `crawler` entry in `compose.yaml`. Gateway's schema and role are provisioned ahead of need; it holds no tables and gets no `DATABASE_URL` yet. Dev-mode entity sync runs as the service role, so it can only create or alter tables in that schema.

## Verify

```sql
SET ROLE crawler_user;
SELECT * FROM llm_router.requests;        -- must fail: permission denied for schema llm_router
SELECT * FROM knowledge_base.documents;   -- must fail: permission denied for schema knowledge_base
SELECT * FROM crawler.pages;              -- must succeed
```
