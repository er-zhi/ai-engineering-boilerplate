# Gateway

The only externally reachable service. Clients hit Gateway over the Connect protocol; Gateway reaches internal services over gRPC. No internal service is published to the host.

Gateway owns no business logic and no markup — it validates input, routes, and maps responses. Page routes are proxied to [Frontend](../frontend/README.md); RPC paths it answers itself. WebSocket entrypoints come later.

## Why Connect and not REST

Browsers cannot speak gRPC: gRPC carries its status in HTTP/2 trailers, and browser JavaScript has no access to trailers. The Connect protocol solves this without giving up proto-defined RPC — a unary call is a `POST` to a path derived from the schema with a JSON body, so a plain `fetch` works with no codegen and no build step.

One `connectrpc` server speaks Connect, gRPC, and gRPC-Web on a single port, so the browser and internal callers share one contract with no proxy in between.

## Security

Single entry point, input validated at the boundary, CORS only where a dev client needs it. Rate limiting and auth are future work.

## Schema (`gateway`)

- `crawl_jobs` — job_id, base_url, include_patterns, exclude_patterns, include_urls, exclude_urls, status, created_at

## API

Methods come from [`common/proto/crawler.proto`](../../common/proto/crawler.proto). Gateway re-exposes `CrawlerService` rather than defining a parallel contract, so there is exactly one schema for both hops.

| Route | Purpose |
|---|---|
| `POST /crawler.v1.CrawlerService/StartCrawl` | Start crawling and indexing a site |
| `POST /crawler.v1.CrawlerService/GetCrawlJob` | Job status and progress |
| `GET /` | Web UI, proxied to Frontend |
| `GET /health` | Plain-HTTP liveness for Compose |

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

`baseUrl` is the crawl root. `scope` glob patterns filter discovered URLs; the explicit URL lists override them. A URL is indexed when it passes the exclude checks and either matches an include pattern or appears in `includeUrls`.

## Page Proxying

Page routes are listed explicitly and forwarded to Frontend over plain HTTP at `FRONTEND_URL`. Everything else falls through to the Connect router, so adding a service to the proto never means editing the route table.

Because pages and RPCs arrive on one origin, no CORS configuration is needed anywhere. That is the whole reason Frontend is not published directly — see [its README](../frontend/README.md).

Serving a page needs Frontend up, so Compose gates Gateway on Frontend's health as well as Crawler's. If Frontend is down, page routes return `502` while RPCs keep working.
