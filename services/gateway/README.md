# Gateway

The only externally reachable service. Clients hit Gateway over the Connect protocol; Gateway reaches internal services over gRPC. No internal service is published to the host.

Gateway owns no business logic and no markup — it routes requests and maps responses. Each service validates what it receives at its own API boundary: Crawler rejects a bad `baseUrl`, and Gateway passes that error through. Page routes are proxied to [Frontend](../frontend/README.md); RPC paths it answers itself. WebSocket entrypoints come later.

## Why Connect and not REST

Browsers cannot speak gRPC: gRPC carries its status in HTTP/2 trailers, and browser JavaScript has no access to trailers. The Connect protocol solves this without giving up proto-defined RPC — a unary call is a `POST` to a path derived from the schema with a JSON body, so a plain `fetch` works with no codegen and no build step.

One `connectrpc` server speaks Connect, gRPC, and gRPC-Web on a single port, so the browser and internal callers share one contract with no proxy in between.

## Security

Single entry point. Rate limiting is future work; authentication is a single shared password gating every page and every RPC. Unlike a per-service DB password, `GATEWAY_AUTH_PASSWORD` is meant to stay fixed and be shared with the whole team — everyone sets the same value in their own `.env`, shared out-of-band, never in git (see `.env.example`).

### Authentication

Not JWT. A JWT is a bearer credential the issuer cannot revoke before it expires and the resource server cannot tell was stolen — self-contained validation is JWT's whole appeal and also the property to avoid here for a single-entry-point gateway that can afford a lookup on every call anyway.

**Opaque session tokens, checked against server-side state on every call.** `POST /login` with `password` (a real HTML form, no JS required) compares it to `GATEWAY_AUTH_PASSWORD` in constant time (both sides hashed with SHA-256 first, then compared byte-by-byte, so neither content nor length leaks through timing), generates a random 256-bit token, and stores its hash — never the raw token — in `gateway.sessions`. The raw token becomes an `HttpOnly; SameSite=Lax` cookie with a `GATEWAY_SESSION_TTL_HOURS`-hour lifetime (default 24). `POST /logout` deletes the row and clears the cookie; an hourly sweep drops rows past their expiry so the table doesn't grow unbounded. A wrong password gets `401`, never which part was wrong.

Storage is Postgres behind the `CacheStore` trait in `common/cache/` (`common::cache::CacheStore`, implemented in `src/sessions.rs` against `gateway.sessions`, its own schema per [gate-database](../../.agents/skills/code-review/gate-database/SKILL.md)) — not an in-process map, so Gateway stays restart- and replica-safe. The trait is generic (`get`/`set`/`delete` on bytes with a TTL); a session row's value is empty, since existence plus `expires_at` is the whole check.

`require_session` wraps every page route and the whole Connect/gRPC fallback in one middleware layer, `/health`, `/login`, and `/logout` excluded. A page request with no valid session redirects to `/login`; an RPC call gets `401` with a Connect-shaped body (`{"code":"unauthenticated",...}`) so the frontend's existing error handling shows it like any other RPC failure.

Two options considered and not built, kept here for when the need actually appears:

- **Sender-constrain the token (DPoP, [RFC 9449](https://datatracker.ietf.org/doc/html/rfc9449))** if a public client beyond this one browser UI is in scope (mobile, an AI agent calling the API): the client proves possession of a private key on every request, so a stolen token alone is inert. OAuth 2.1 and MCP both list this as the recommended hardening for public clients in 2026 — [WorkOS: DPoP explained](https://workos.com/blog/dpop-rfc-9449-explained), [Kong: DPoP](https://konghq.com/blog/engineering/demonstrating-proof-of-possession-dpop-preventing-illegal-access-of-apis). A confidential client (server-to-server) reaches for mutual TLS instead ([RFC 8705](https://datatracker.ietf.org/doc/html/rfc8705)).
- **PASETO over hand-rolled JWT-shaped tokens** for anything that must stay self-contained regardless (e.g., a short-lived proof passed to a service that cannot do the session lookup) — PASETO versions remove the algorithm-confusion and `alg: none` failure modes JWT libraries have to guard against by convention rather than by design.

Only Gateway checks the session, per [gate-architecture](../../.agents/skills/code-review/gate-architecture/SKILL.md#boundaries) — internal services keep trusting the request the way they do today, validating their own inputs, not who sent them.

## API

Methods come from [`common/proto/crawler.proto`](../../common/proto/crawler.proto). Gateway re-exposes `CrawlerService` rather than defining a parallel contract, so there is exactly one schema for both hops.

| Route | Purpose |
|---|---|
| `POST /crawler.v1.CrawlerService/StartCrawl` | Start crawling and indexing a site; repeat it safely with the same `idempotencyKey` |
| `POST /crawler.v1.CrawlerService/GetCrawlJob` | Job status and progress |
| `POST /crawler.v1.CrawlerService/GetPageNeighbors` | Pages reachable from a crawled URL via discovered links, for an agent deciding what to fetch next |
| `POST /knowledge_base.v1.KnowledgeBaseService/Ingest` | Store enriched, embedded content; crawler is the only caller today |
| `POST /knowledge_base.v1.KnowledgeBaseService/Search` | Ranked documents over what has been ingested, each with its best-matching passage |
| `GET /`, `GET /sources` | Web UI: a retrieval test page, and the crawl-starting form, served from Frontend's build |
| `GET /login`, `POST /login` | Login form, and the password check that starts a session |
| `POST /logout` | Ends the current session |
| `GET /health` | Plain-HTTP liveness for Compose — the only page route that skips the session check |

Every route above except `/health`, `/login`, and `/logout` requires a valid session — see [Authentication](#authentication).

A call carries `content-type: application/json` and `connect-protocol-version: 1`. Field names use protobuf canonical JSON, so `base_url` is `baseUrl` on the wire, and enums are their value names.

```bash
curl -X POST localhost:8080/crawler.v1.CrawlerService/StartCrawl \
  -H 'content-type: application/json' -H 'connect-protocol-version: 1' \
  -d '{"baseUrl":"https://example.com","scope":{"excludePatterns":["*/admin/*"]}}'
```

```json
{ "jobId": "job-1", "status": "CRAWL_STATUS_QUEUED" }
```

Errors arrive as a Connect error body with a real HTTP status — `404` with `{"code":"not_found"}` for an unknown job, `400` with `{"code":"invalid_argument"}` for a missing `baseUrl`. Unlike gRPC-over-HTTP/2, the status code is meaningful to browsers and proxies.

`baseUrl` is the crawl root; what `scope` does is described in [Crawler's README](../crawler/README.md#scope).

## Page Proxying

Gateway holds no runtime dependency on Frontend. Frontend's one-shot build container copies `client/` into a Docker volume and exits (`service_completed_successfully` gates Gateway's startup on that, not a health check); Gateway mounts the same volume read-only and serves `index.html` and `sources.html` straight off disk with `tower_http::services::ServeFile`, one route per page, no proxy, no per-request call to Frontend at all. A page request reads the current file on every load — there is no in-process cache to go stale, and a Frontend container that never runs again cannot 502 a page, because nothing calls it once its files are written.

`Cache-Control: no-store` on every page route (see [Authentication](#authentication) below for why this replaced an earlier plan to cache pages for 5 minutes). Everything else falls through to the Connect router, so adding a service to the proto never means editing the route table. Because pages and RPCs arrive on one origin, no CORS configuration is needed anywhere — see [Frontend's README](../frontend/README.md) for why Frontend is never reachable directly.

Every call to Crawler and knowledge-base carries a 10 s deadline, so a hung internal service fails the one request instead of pinning a Gateway connection forever.

### Why Pages Are `no-store`, Not Cached

The original plan cached every page for 5 minutes, on the reasoning that Gateway serving from disk on every request was needless work for content that rarely changes. Authentication changed that: every page behind `require_session` is gated per-session, and a client-side cache would let a browser show a stale authenticated page after logout without ever asking the server again — confirmed live: with `max-age=300` in place, logging out and revisiting `/` in the same browser session served the cached authenticated page straight from the browser's own HTTP cache, no request to Gateway at all, so the session check never ran. `no-store` forces every page load to hit the server, where `require_session` decides fresh each time.

This applies to the HTML entry points only. A future bundler's content-hashed assets (`app.a3f1c2.js`) are not session-specific — the same bytes for every session — so caching those forever is still safe and still the plan once Frontend's output is more than one file; see [Frontend's Assets section](../frontend/README.md#assets).
