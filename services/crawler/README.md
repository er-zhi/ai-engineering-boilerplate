# Crawler

Crawls public websites with [spider-rs](https://github.com/spider-rs/spider), extracts readable content, persists crawl state, records discovered links, and submits stored pages to [Knowledge Base](../knowledge-base/README.md).

## Current Behavior

- `StartCrawl` accepts an absolute public HTTP or HTTPS URL. Loopback, private, link-local, and `localhost` targets are rejected to prevent SSRF.
- Crawling uses spider-rs `crawl()` over plain HTTP, stays on one host, respects `robots.txt`, and applies a 15-second request timeout.
- A response body is capped at 5 MiB. A crawl counts at most `CRAWL_MAX_PAGES` pages: the code defaults to 100, the provided `.env.example` selects 10,000 for Compose, and values above 10,000 are rejected.
- Main text is extracted with `dom_smoothie`; raw HTML is not stored.
- `crawler.pages` stores one row per URL with title, main text, content hash, HTTP status, and crawl time.
- `crawler.page_edges` stores links observed between fetched pages for `GetPageNeighbors`.
- Each successfully stored page is sent to `knowledge-base.Ingest`, including unchanged pages. Knowledge Base performs the authoritative deduplication, so a later crawl can retry a previously failed hand-off without repeating paid work.
- Storage, graph, or Knowledge Base hand-off errors are logged for the affected page and do not currently fail the whole crawl. `pages_crawled` counts successfully stored pages.

Jobs move from `QUEUED` to `RUNNING`, then `DONE` or `FAILED`. An optional `idempotency_key` returns the existing job when the same request is retried. At most two crawls execute concurrently; additional jobs remain queued. A PostgreSQL advisory lock ensures only one Crawler process owns the queue, and unfinished jobs are marked failed when a new owner takes over.

The pipeline uses bounded channels with capacity one. If a stage loses a fetched page, the job fails instead of silently reporting incomplete success. `pages_skipped` is reserved for future crawl-side skipping and currently remains zero.

## Scope

| Field | Effect |
|---|---|
| `exclude_patterns`, `exclude_urls` | Prevent a URL from being fetched, so its links are not discovered. |
| `include_patterns`, `include_urls` | Decide which fetched pages are stored; they do not restrict link traversal. |

A scope accepts at most 100 rules across all four lists and at most 2,048 characters per rule. Empty include lists accept every fetched page. `*` matches any character sequence. A pattern beginning with `/` matches the URL path; other patterns match the complete URL. URL entries are exact matches.

## API and Storage

The current contract is [`crawler.proto`](../../common/proto/crawler/v1/crawler.proto):

- `StartCrawl` starts a background crawl.
- `GetCrawlJob` returns status and counters.
- `GetPageNeighbors` returns discovered outgoing links for a stored page.
- `GET /health` is a plain liveness endpoint.

The entity definitions in [`src/entity/`](src/entity/) are the source of truth for the `crawler` schema: `crawl_jobs`, `pages`, and `page_edges`.

Run tests with `cargo nextest run -p crawler`. Network tests use a local server; database tests use PostgreSQL through testcontainers.
