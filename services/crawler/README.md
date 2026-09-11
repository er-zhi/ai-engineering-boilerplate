# Crawler

Crawls websites and indexes their content for hybrid semantic search. Crawler library: [spider-rs](https://github.com/spider-rs/spider).

## What Runs Today

The first slice crawls for real but stores nothing yet:

- `StartCrawl` checks that `base_url` is an absolute http(s) URL, returns `QUEUED`, and crawls in the background with spider-rs `crawl()` — plain HTTP, no Chromium.
- The job moves `QUEUED → RUNNING → DONE`, and `pages_crawled` rises as pages arrive. It ends `FAILED` when not a single page could be fetched.
- One host only, `robots.txt` respected, 15 s per request, at most `CRAWL_MAX_PAGES` fetched pages per crawl (default 100).
- `pages_skipped` stays `0` until the skip ladder below exists; it counts unchanged pages, which needs storage.
- Jobs live in memory and vanish on restart. Nothing past **Fetch** in the pipeline below runs yet.

### Scope

| Field | Effect |
|---|---|
| `exclude_patterns`, `exclude_urls` | Never fetched, so links on those pages are never followed either |
| `include_patterns`, `include_urls` | Decide which fetched pages count. They do not limit link-following, so in-scope pages reachable only through other pages are still found |

Both include lists empty means every fetched page counts. `*` matches any run of characters. A pattern starting with `/` matches from the URL path (`/docs/*`); any other pattern must match the whole URL (`*/admin/*`). `*_urls` entries match exactly.

Tests: `cargo nextest run -p crawler`. Crawl tests run against a local site on `127.0.0.1`, never the internet.

## Pipeline

1. **Plan** — split URLs into known revisits and new discoveries; drop any whose sitemap `lastmod` predates `crawled_at`
2. **Fetch** — conditional GET for revisits, `crawl_smart()` for discoveries; a `304` ends the work for that page
3. **Denoise** — strip nav, ads, footers; keep main body only
4. **Compare** — hash the denoised content; an unchanged hash ends the work before any spend
5. **Enrich** — LLM Router (`low`/`medium` tier) returns page type, keywords, summary
6. **Embed** — embedding model produces the content vector
7. **Store** — extracted content only; raw HTML is never persisted
8. **Search** — pgvector similarity combined with keyword and type filters

## Skip Ladder

Never re-do work for a page that has not changed. Each check exists to avoid the next, more expensive step, so the order is the design — a later check cannot recover the cost of an earlier one already paid.

| Check | Cost avoided |
|---|---|
| Sitemap `lastmod` older than `crawled_at` | The request itself — one sitemap covers many URLs |
| `304 Not Modified` | Body transfer, parsing, and any Chromium call |
| Denoised hash unchanged | LLM enrichment and embedding — the only steps that cost money |

Conditional GET sends `If-None-Match` with the stored `etag`, falling back to `If-Modified-Since` with `last_modified` when no ETag exists. Do not send both: per RFC 9110 §13.1.3 a recipient MUST ignore `If-Modified-Since` whenever `If-None-Match` is present, so the date is dead weight rather than a fallback.

## Two Phases

Revisits and discovery use different tools, because spider-rs `with_headers` applies to the whole website rather than to one request — it cannot carry a distinct `If-None-Match` per URL.

- **Revisit** known URLs directly with `reqwest` (re-exported as `spider::reqwest`, so no new dependency), one conditional request per URL with its own validator. Spider and Chromium are never involved.
- **Discover** new URLs with spider-rs `crawl_smart()` over seeds, sitemaps, and the link graph. New pages have no stored validator, so the website-level header limit does not bite.

Most of a re-crawl is revisits, which is why that path stays on the cheapest possible mechanism.

Hash the **denoised** content, not the raw HTML. Raw HTML churns on every fetch through ads, timestamps, and session tokens while the actual article is untouched; hashing after denoise is what keeps cosmetic churn from triggering LLM and embedding spend. One hash is enough — denoise is local CPU, so a second hash to guard it would buy nothing.

Pages that repeatedly come back unchanged earn a longer interval: increment `unchanged_streak`, push `next_crawl_at` out exponentially, and reset both on a real change. A static page should converge toward being checked rarely.

## Chromium

Chromium dominates crawl cost, so treat it as an escalation and never a default.

- Crawl with spider-rs `crawl_smart()` (`smart` feature), which issues plain HTTP first and upgrades to Chrome only when a page actually needs JavaScript to render.
- Record the outcome per URL in `needs_js`. Pages known to render over HTTP never attempt Chrome again; pages known to need JS skip the wasted HTTP attempt.
- Reuse one browser instance across pages. Process launch, not page rendering, is the bulk of the cost.
- The ladder above runs before any of this, so a `304` never reaches Chromium at all.

## Schema (`crawler`)

- `pages` — url, title, main_content, page_type, keywords, summary, metadata, content_hash, crawled_at
- `pages` freshness — `etag varchar(256)`, `last_modified timestamptz`, `needs_js boolean`, `unchanged_streak smallint`, `next_crawl_at timestamptz`
- `page_embeddings` — page_id, embedding, model_version

`etag` and `last_modified` are nullable — plenty of servers send neither, and those pages fall back to the hash check.

## Page Types

LLM picks the best match: `product`, `knowledge`, `instruction`, `documentation`, `blog`, `other`.

Enrichment result per page:

```json
{
  "page_type": "product",
  "keywords": ["wireless headphones", "noise cancelling"],
  "summary": "Product page for Sony WH-1000XM5 headphones"
}
```

## API

Defined in [`common/proto/crawler.proto`](../../common/proto/crawler.proto) and served over both Connect and gRPC; Gateway reaches it over gRPC.

- `StartCrawl` — submit a base URL and scope to crawl and index
- `GetCrawlJob` — job status and page counts
- `Search` — semantic similarity plus lexical and type filters (not yet implemented)
- `GET /health` — plain-HTTP liveness for Compose
