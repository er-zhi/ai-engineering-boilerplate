# Crawler Page Storage — Design

Date: 2026-09-11. Status: approved in chat, pending review of this document.

## Goal

A crawl saves every in-scope page it fetches to Postgres, as extracted main text rather than raw HTML, so the next steps (search, LLM enrichment, embeddings) have something to read. Today the crawler only counts pages, and all service schemas are empty.

## Scope

In:

- A `crawler.pages` table, created from a SeaORM entity by schema sync on startup.
- Main-text extraction per page, then an insert or update keyed by URL.
- `pages_crawled` counts pages actually written.
- The crawler connects to the Compose Postgres as its own role.

Out, for later slices: skip-ladder columns (etag, last_modified, needs_js, unchanged_streak, next_crawl_at), LLM enrichment and page type, embeddings, search, persisting jobs (they stay in memory), and production migrations.

## Decisions

| Decision | Choice | Why |
|---|---|---|
| Database layer for every service | SeaORM 2 (2.0.2), entity-first | `goal.md`: entities are the source of truth and the schema syncs from them. Sync never drops tables or columns. |
| Main-text extraction | `dom_smoothie` 0.18 | Actively maintained Readability port: returns the title and main text with nav, ads, and footers removed. |
| Content hash | SHA-256 hex of `main_text` (`sha2` crate) | The skip ladder compares hashes of the denoised text. Fits the gate's `char(64)` example. |

## Architecture

```
spider crawl ──(counted page: url, html, status)──▶ channel ──▶ store task ──▶ crawler.pages
                                                                     │
                                                                     └──▶ job.pages_crawled += 1 after each write
```

- `crawl.rs` stays database-free. Instead of calling `on_counted(url)`, it sends each counted page into a channel. The crawl tests keep running with no database.
- New `store.rs` owns extraction and persistence: HTML → `dom_smoothie` → title and text → SHA-256 → insert or update by `url`.
- New `entity/page.rs` defines the table. On startup, `main.rs` connects with `DATABASE_URL` and runs `db.get_schema_registry("crawler::entity::*").sync(db)`.
- `jobs.rs` drives both: it runs the crawl and the store task, and advances the count as rows land.

## Table `crawler.pages`

| Column | Type | Notes |
|---|---|---|
| `id` | `bigserial` primary key | Internal id |
| `url` | `varchar(2048)` not null, unique | Key for insert-or-update |
| `title` | `varchar(512)` not null | Cut to 512 characters. Empty when the page has none |
| `main_text` | `text` not null | Extracted article text. Empty when extraction finds no article |
| `content_hash` | `char(64)` not null | SHA-256 hex of `main_text` |
| `http_status` | `smallint` not null | Status of the fetch |
| `crawled_at` | `timestamptz` not null | Time of the last write |

The entity sets `schema_name = "crawler"`, so tables land in the crawler's schema no matter what the connection's `search_path` is. No raw HTML is ever stored.

## Behavior

- **Re-crawl:** a URL seen again updates its row (title, text, hash, status, time), so there are never duplicates.
- **No article found** (`GrabFailed`): the page is still saved, with its `<title>` if present and empty `main_text`, and still counts.
- **Write fails:** the error goes to stderr (logs never go in the database), the page is not counted, and the crawl continues. The job ends `DONE` when the crawl finishes. It ends `FAILED` only when not a single page could be fetched, as today.
- **Database unreachable at startup:** the crawler exits with an error. Compose starts it only after Postgres is healthy.

## Configuration

- Compose gives the crawler `DATABASE_URL=postgres://crawler_user:${CRAWLER_DB_PASSWORD}@postgres:5432/app` and `depends_on: postgres: service_healthy`.
- Schema sync runs on every startup. This departs from `goal.md` ("migration files only for prod") because there is no production deployment yet. The README will state that migrations must replace startup sync before real data.

## Testing

- **Unit** (no database): extraction on a fixture page with nav, article, and footer keeps the article text and drops the rest. The title is picked up, and the hash equals a SHA-256 value computed by hand for a literal string.
- **Integration** (real Postgres through testcontainers 0.28, `pgvector/pgvector:pg18`):
  - After sync, `information_schema` reports exactly `varchar(2048)`, `varchar(512)`, `text`, `char(64)`, `smallint`, `timestamptz`, and a unique constraint on `url`.
  - Storing the same URL twice leaves one row with the second write's title and hash.
  - A crawl of the local test site stores exactly the in-scope pages, and `pages_crawled` equals the stored row count.
- **Runner:** the nextest container gets the Docker socket and `--network host`, so testcontainers can start Postgres and reach its mapped port. Both were verified on this machine (OrbStack).
- The existing 20 tests stay database-free and must keep passing. clippy with `-D warnings` and `cargo fmt --check` must stay clean.

## Done When

- The tests above pass.
- `docker compose up` plus a crawl from the browser leaves rows in `crawler.pages`, visible from a database client connected as `postgres`.
- Crawling the same site again updates those rows instead of duplicating them.
