# Frontend

Frontend has no runtime server reachable by anything at request time. Its job is to prepare the browser bundle and write it to a folder Gateway serves; nothing calls Frontend while the stack is running, and Frontend calls nothing else — not Crawler, not LLM Router, not any service. Every page, style, and line of client-side logic still lives here; no markup or browser JavaScript belongs anywhere else.

## Never Straight to a Service

[gate-architecture](../../.agents/skills/code-review/gate-architecture/SKILL.md) rules out cross-service coupling outside gRPC contracts. Frontend has no contract at all: it produces files, not responses to requests. If a future build step ever needs live data from another service (an API base URL, a feature flag), that call goes through Gateway the same way a browser's would — Frontend never opens a socket to Crawler or LLM Router directly.

## Build Once, Serve From Gateway

Frontend's container copies `client/` into a folder shared with Gateway and exits; it does not stay up answering HTTP. Gateway serves that folder straight to the browser with `Cache-Control: no-store` — every page is behind a session (see [Gateway's README](../gateway/README.md#why-pages-are-no-store-not-cached)), so nothing about the current pages is safe to cache client-side; a future bundler's content-hashed assets are the exception, once there are any.

This removes the internal HTTP hop the previous per-request proxy design paid on every page load, and the runtime dependency that came with it — a Frontend that is not running cannot 502 a page, because nothing calls it while the stack serves traffic.

## Why Gateway, Never a Direct Path

The browser must load the page and call RPCs from a single origin, otherwise every Connect call pays a CORS preflight — Connect sends `connect-protocol-version`, a custom header, so no call qualifies as a simple request. Gateway stays the browser's only way in. Frontend is never published to the host either way; the change here is that it no longer runs at all once its output is written.

## Pages

| Route | Purpose |
|---|---|
| `GET /` | A test page for knowledge-base retrieval — what `Search` returns for a query, with a page-type filter; a new query aborts the previous one; every field rendered as text, never as HTML. knowledge-base's real callers are agents, not this page |
| `GET /sources` | Add a source — start a crawl and watch job status; one source among possibly several the knowledge base can be fed from |
| `GET /login` | The password form that starts a session |

Every page above except `/login` requires a session; see [Gateway's Authentication section](../gateway/README.md#authentication). `GET /health` no longer applies to Frontend once it stops running as a service; Gateway's own `/health` covers the externally visible liveness check.

## Assets

No bundler yet — `client/` holds a handful of static HTML files, so today's "build" is a copy, not a compile: Frontend's build step copies `client/` into the shared output folder as-is. `docker compose up --watch` re-triggers that copy on every save, the same as it rebuilds any other service's image. When the UI grows enough to need a real bundler (esbuild, Vite, whatever fits then), the same copy step ships that bundler's output directory instead — Gateway's side of the contract, a folder of static files, does not change.

Once that bundler exists, its output filenames should be content-hashed (`app.a3f1c2.js`, not `app.js`) so Gateway can cache them forever (`Cache-Control: max-age=31536000, immutable`) rather than `no-store` like the HTML entry points — a hashed asset is not session-specific, so caching it is safe in a way caching a page behind auth is not. See [Gateway's Page Proxying section](../gateway/README.md#page-proxying) for the entry-point side of that split.

## Serving Is Gateway's Job

Gateway reads the shared output folder and serves it directly — no proxy, no per-request call to Frontend. See [Gateway's README](../gateway/README.md#page-proxying) for the serving side: the route table, the cache header, and what happens on a stale or missing build.

## Talking to the Backend

The page calls Connect endpoints exposed by Gateway at relative paths, so it inherits whatever origin served it — this is unaffected by how the HTML itself reached the browser:

```javascript
fetch("/crawler.v1.CrawlerService/StartCrawl", {
  method: "POST",
  headers: { "content-type": "application/json", "connect-protocol-version": "1" },
  body: JSON.stringify({ baseUrl }),
});
```

Never hardcode a host or port. Relative paths are what keep the single-origin property true in dev, in Compose, and behind a real domain later.

Headers, field naming, and error shapes are described once, in [Gateway's README](../gateway/README.md#api).
