# Gateway

The only public application service. Gateway authenticates browser sessions, serves Frontend's static files, exposes selected Connect RPCs, and calls internal services over gRPC. Business validation remains in the service that owns the capability.

## Authentication

`POST /login` checks `GATEWAY_AUTH_PASSWORD`, creates a random opaque session token, stores only its hash in `gateway.sessions`, and sets an `HttpOnly; SameSite=Lax` cookie. Sessions expire after `GATEWAY_SESSION_TTL_HOURS` (24 by default). `POST /logout` deletes the session and clears the cookie; an hourly task removes expired rows.

Every page and RPC is checked against PostgreSQL on each request. `/health`, `/login`, and `/logout` are the only unauthenticated routes. Unauthenticated page requests redirect to `/login`; RPC requests receive a Connect-shaped `unauthenticated` response.

Gateway uses `common::cache::CacheStore` for session storage. PostgreSQL is the current and only backend.

## Public Surface

| Route | Behavior |
|---|---|
| `GET /`, `/sources`, `/login` | Serve Frontend files from `FRONTEND_DIST_DIR`. |
| `POST /login`, `/logout` | Create or remove a session. |
| Crawler `StartCrawl`, `GetCrawlJob`, `GetPageNeighbors` | Proxy to Crawler. |
| Knowledge Base `Search` | Proxy to Knowledge Base. |
| Knowledge Base `Ingest` | Rejected as unimplemented; ingestion is an internal Crawler-to-Knowledge-Base operation. |
| `GET /health` | Plain liveness response. |

Public RPC paths and JSON fields come directly from the proto contracts in [`common/proto/`](../../common/proto/). The browser sends `content-type: application/json` and `connect-protocol-version: 1` and uses protobuf JSON field names.

Gateway gives upstream calls a 10-second deadline. Read-only Crawler calls (`GetCrawlJob` and `GetPageNeighbors`) retry transient unavailable/deadline failures up to three attempts; `StartCrawl` is not retried because it triggers work. Knowledge Base search is not retried.

## Static Files and Configuration

Frontend's one-shot Compose container must complete before Gateway starts. Gateway reads the shared output directory directly; it does not proxy to a Frontend server. Pages use `Cache-Control: no-store`, ensuring logout and session expiry take effect on the next load.

Required configuration:

- `GATEWAY_AUTH_PASSWORD`
- `DATABASE_URL`

`FRONTEND_DIST_DIR` (default `/frontend`), `CRAWLER_URL` (default `http://127.0.0.1:8081`), `KNOWLEDGE_BASE_URL` (default `http://127.0.0.1:8084`), and `GATEWAY_SESSION_TTL_HOURS` (default 24) are optional. Only port 8080 is published for application traffic.
