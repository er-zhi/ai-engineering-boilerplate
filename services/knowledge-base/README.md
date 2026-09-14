# Knowledge Base

Stores what a source has produced about one piece of content — its full text, an LLM-derived page type, keywords, and summary, and the embedded passages the text is split into — and serves hybrid retrieval over it through `Search`. Its callers are other services, typically AI agents: `Search` returns the passages and their sources, and the agent writes any answer itself with its own model.

## Why a Separate Service

Crawling and knowledge storage are different capabilities with different lifecycles. Crawler keeps its own crawl record and content hash; Knowledge Base is the canonical, source-agnostic retrieval store. Crawler is the current caller, but another source can use the same `Ingest` contract.

## Content Ownership and Dedup

Knowledge Base never trusts a caller's dedup claim. It hashes the `content` it receives and upserts by `(source, source_id)`; if the hash matches what is already stored, it returns `skipped: true` without spending anything on the LLM or the embedding model. Crawler records its own content hash but submits every successfully stored page. This service's check is authoritative and also makes a later crawl a safe retry after a failed hand-off.

## Pipeline per `Ingest` Call

1. Hash `content`; if it matches the stored hash for `(source, source_id)`, stop here.
2. `llm-router.Complete` (`low` tier, `response_format: json_object`) for `page_type`, `keywords`, and `summary`. Supported page types are `product`, `knowledge`, `instruction`, `documentation`, `blog`, and `other`; an unrecognized value becomes `other`.
3. Split `content` into passages of at most 1,200 characters (`chunk::split`), breaking on lines, then sentences, then words, with no overlap — in Chroma's chunking evaluation, overlap cost precision without improving recall.
4. Embed each passage with a **contextual chunk header** prepended — the first 150 characters of the title and the first 300 of the summary — over loopback HTTP to the native `embedder-ane` process. The header is embedded, never stored, so a passage deep in a page still carries what the page is about, at no extra LLM cost. (It is not Anthropic's Contextual Retrieval, which has an LLM write per-chunk context; that is heavier and remains an option.) See [Embedding](#embedding-native-apple-silicon-only) below.
5. Write the document and all its passages in one transaction, replacing any passages from a previous version. Either the whole call succeeds or nothing beyond what was already there is written.

A failure at step 2 or 4 fails the whole call; there is no partial write and no durable retry queue. Crawler logs a failed hand-off and moves on. Its next successful crawl submits the page again, and Knowledge Base either ingests it or skips it using its own hash.

## Embedding: Native, Apple Silicon Only

This is the one place knowledge-base talks to something other than llm-router or its own database, and it is a deliberate, documented exception rather than an oversight: `embedder_client.rs` sends the text over plain HTTP to [`native/embedder-ane`](../../native/embedder-ane/README.md), a process that runs directly on the host (never in Docker) because it needs Apple's Core ML to reach the Neural Engine, which has no Linux/container equivalent. `knowledge-base`'s own container reaches it at `EMBEDDER_URL` (default `http://host.docker.internal:8086`).

Two endpoints, not one: `POST /embed/document` for passages written at ingest, `POST /embed/query` for a query — the model (`google/embeddinggemma-300m`) needs a different task prefix for each, per its own documented usage pattern, and getting that backwards would quietly hurt ranking rather than error. `EmbedKind::Document` / `EmbedKind::Query` on `LlmClient::embed` is what picks the route.

The embedder's window is a fixed 128 tokens — deliberately small in exchange for Neural Engine speed (see [`native/embedder-ane`](../../native/embedder-ane/README.md)). Passage (`chunk::MAX_CHUNK_CHARS`) and header (`MAX_HEADER_TITLE_CHARS` + `MAX_HEADER_SUMMARY_CHARS`) sizes are set together so a typical English passage plus its header stays comfortably under that, verified against the real tokenizer; denser text such as non-Latin scripts, code, or URLs can still exceed it. In that case the embedder uses the first 128 tokens and `embedder_client` logs a warning. Passage sizing is character-based, not tokenizer-based. The embedder call has its own 10-second timeout, separate from LLM Router's 60-second timeout.

No request or payload logging is kept for these calls, unlike `llm-router`'s `requests` and `request_payloads` tables: the forward pass is local and unmetered. Model and runtime details are in the [`native/embedder-ane` README](../../native/embedder-ane/README.md).

## Retrieval

`Search` (`service::retrieve`) is hybrid retrieval over passages with Reciprocal Rank Fusion, the fusion Elasticsearch's `rrf` retriever and Supabase's hybrid-search guide use:

1. Normalize the query once (`search::normalize`: whitespace collapsed, lowercased).
2. Semantic and lexical retrieval run concurrently with `tokio::join!`; this caps each search at two database queries in flight. The lexical branch runs its two short, independently indexed queries sequentially and produces three candidate lists in total, each capped at 50:
   - **Semantic** — `store::nearest`: `ORDER BY embedding <=> $query`, served by the HNSW index. Finds passages that mean the same thing in different words, or in another language.
   - **Passage lexical** — `store::lexical_passages`: passage content matched and ranked with `to_tsquery` and `ts_rank_cd`.
   - **Title lexical** — `store::lexical_titles`: document titles matched and ranked independently with the same full-text primitives, returning one deterministic fallback passage per document. Keeping the lexical predicates in separate queries lets PostgreSQL use each table's GIN index instead of evaluating a cross-table `OR`.
   The lexical query uses any-word matching (`word | word | …`), not the all-words `plainto_tsquery`, because a natural question rarely has every word in one passage; passages matching more terms still rank higher. It finds exact terms, product names, and identifiers that embeddings blur. Candidate queries project only response fields, not full documents or stored vectors.
3. **Reciprocal Rank Fusion** (`store::fuse_with_document_titles`): semantic and passage-text rank are fused per passage. A title rank boosts the best retrieved passage from that document; the deterministic first passage is used only when the document was found by title alone. Each contribution is `1/(60 + rank)` (k = 60 from Cormack, Clarke & Büttcher 2009). Only rank order matters, so a cosine distance and a `ts_rank_cd` never have to share a scale.

pgvector's HNSW scan reads only `hnsw.ef_search` candidates (default 40) and applies a `WHERE` filter afterwards, which would silently return fewer than 50 passages — far fewer with a page-type filter. The `knowledge_base_user` role therefore defaults to `hnsw.ef_search = 100` and `hnsw.iterative_scan = strict_order`, set in Compose's Postgres bootstrap.

An optional `page_types` filter restricts all retrievers. Page type is a facet the caller chooses, never something guessed from the query text and mixed into the score.

**`Search`** keeps each document once, represented by its best passage (`store::best_per_document`), and returns the top `limit` (10 by default, at most 50) with that passage as `snippet`, its ordinal, a versioned `DocumentRef`, and the document's `updated_at`. An agent can judge freshness from the compact result and call `ReadDocument` only when it needs the complete source.

`ReadDocument` returns bounded, character-safe pages from the canonical full text. The opaque cursor is valid only together with the `DocumentRef` returned by `Search`; if the document has changed, the read fails explicitly and the agent must search again. This prevents an agent from silently combining passages from different document versions.

There is no generation step here. An agent calling `Search` already has a model; a second LLM inside knowledge-base would add seconds and cost for text the agent rewrites anyway. The web page at `/` only exists to inspect what `Search` returns.

Current limitations: there is no labelled retrieval evaluation set, cross-encoder reranker, or true BM25. Lexical ranking uses PostgreSQL `ts_rank_cd`.

## Foundation for Agentic RAG

Knowledge Base is the retrieval layer for a future adaptive, self-correcting RAG loop; it is not itself an answer-generating agent. The intended flow is:

`Query -> reasoning/orchestration agent -> Search -> ReadDocument when needed -> evidence check -> another retrieval or tool call when needed -> final answer`

The orchestrator may later route individual steps to web search, a knowledge graph, or other tools and ask its model whether the gathered evidence is sufficient. Those capabilities belong behind separate adapters so internal KB retrieval remains fast, deterministic, independently testable, and usable without an LLM call. A knowledge graph should be added for workloads that need entity relationships, not as a mandatory hop for every query.

Agents should start with `Search`, keep the returned source metadata and `DocumentRef` as citations, then page through `ReadDocument` only for sources that require broader context. This small-to-big path avoids placing every full document in the model context while still allowing exact whole-document inspection.

## Testing

Unit tests use a fake `LlmClient` — no real HTTP call to the embedder or llm-router. `store` and entity tests run against a real Postgres via `test_db`. Search performance has two fast regression guards: a synchronization barrier proves semantic and lexical retrieval start concurrently, while a warmed service-level search with real PostgreSQL has a deliberately loose one-second ceiling that catches operational stalls with headroom for normal CI variance. A SQL-shape test separately prevents passage and title predicates from being joined back into the cross-table `OR` that caused the measured regression.

## gRPC API

[`common/proto/knowledge_base/v1/knowledge_base.proto`](../../common/proto/knowledge_base/v1/knowledge_base.proto):

- `Ingest(source, source_id, title, content) -> (stored, skipped)`. `source` and `source_id` together identify one document; `source` names the caller (`"crawler"` today), `source_id` is whatever that caller uses to identify the thing (crawler uses the URL).
- `Search(query, page_types, limit) -> results[]` — up to `limit` documents (0 means 10, at most 50), each with its best-matching `snippet`, `passage_ordinal`, versioned `document` reference, and `updated_at` (RFC 3339). At most 6 page types; a query up to 1,000 characters. The request carries only what agent-facing retrieval tools commonly expose — result count and a metadata filter; search mode, fusion constants, and thresholds stay server-side.
- `ReadDocument(document, cursor, max_chars) -> (content, next_cursor, total_chars)` — reads one exact document version in bounded pages. `max_chars` defaults to 16,000 and cannot exceed 50,000; an empty `next_cursor` means the document is complete. Callers treat cursors as opaque and restart from `Search` when the version is stale.

`Ingest` accepts `content` up to 200,000 characters — about 170 passages, a few seconds of embedding — so one call stays well inside crawler's 90 s timeout.

`GET /health` is plain liveness; it does not call llm-router or Postgres.

## Schema (`knowledge_base`)

- `documents` — `source`, `source_id` (unique together), `title`, `content` (complete, untruncated), `content_hash`, `page_type` (smallint discriminant), `keywords` (`varchar(64)[]`), `summary`, `embedding_model`, `ingested_at`, `updated_at`
- `document_chunks` — `document_id` (foreign key, `ON DELETE CASCADE`), `ordinal` (unique with `document_id`), `content`, `embedding` (`vector(768)`)

Indexes, created at startup after schema sync: HNSW on `document_chunks.embedding` (cosine), GIN on `to_tsvector('english', document_chunks.content)`, GIN on `to_tsvector('english', documents.title)`.

`embedding_model` is stored per document so a future switch of the embedding model — a different vector space — is detectable rather than silently corrupting similarity search. `ingested_at` is set once, on the first write for a `(source, source_id)`.

Schema is synced by SeaORM at startup, non-destructively: a column whose type no longer matches the entity is logged, not altered, and a column removed from an entity stays in the table. There is no migration mechanism yet, so a database created before passage storage existed must be reset by hand; its old `documents.embedding NOT NULL` column otherwise fails every write:

```bash
docker compose exec -T postgres psql -U postgres -d app \
  -c "DROP TABLE IF EXISTS knowledge_base.documents CASCADE" \
  -c "TRUNCATE crawler.pages, crawler.crawl_jobs RESTART IDENTITY CASCADE" \
  -c "ALTER ROLE knowledge_base_user SET hnsw.ef_search = 100" \
  -c "ALTER ROLE knowledge_base_user SET hnsw.iterative_scan = strict_order"
docker compose restart knowledge-base
```

Then start a crawl again; knowledge-base recreates both tables and their indexes on startup.

## Config

`LLM_ROUTER_URL` (default `http://127.0.0.1:8083`), `EMBEDDER_URL` (default `http://host.docker.internal:8086` — see [Embedding](#embedding-native-apple-silicon-only)), and `DATABASE_URL`. This service holds no provider credentials — it never sees `OPENROUTER_API_KEY`, only llm-router does.
