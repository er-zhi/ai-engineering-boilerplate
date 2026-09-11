# Gateway

The only externally reachable service. Clients hit Gateway over REST; Gateway reaches internal services over gRPC. No internal service is published to the host.

Gateway owns no business logic — it validates input, routes, and maps responses. gRPC and WebSocket entrypoints come later.

## Security

Single entry point, input validated at the boundary, CORS only where a dev client needs it. Rate limiting and auth are future work.

## Schema (`gateway`)

- `crawl_jobs` — job_id, base_url, include_patterns, exclude_patterns, include_urls, exclude_urls, status, created_at

## API

- `POST /api/crawl` — start crawling and indexing a site
- `GET /api/crawl/:job_id` — job status and progress
- `GET /api/search?q=` — proxied to Crawler
- `GET /health`

### `POST /api/crawl`

```json
{
  "base_url": "https://example.com",
  "include_patterns": ["/docs/*", "/blog/*"],
  "exclude_patterns": ["*/admin/*", "*/login"],
  "include_urls": ["https://example.com/pricing"],
  "exclude_urls": ["https://example.com/private/page"]
}
```

`base_url` is the crawl root. Glob patterns filter discovered URLs; the explicit URL lists override them. A URL is indexed when it passes the exclude checks and either matches an include pattern or appears in `include_urls`.

## Dev Client

`client/index.html` — static, no build step, served at `/` in dev mode. Form fields map one-to-one to the request body above, plus job status polling and a search box.

The filename stays `index.html` — that is the web convention for a root document, unrelated to the service name.
