# Frontend

Owns the web UI. Every page, style, and line of client-side logic lives here — no markup or browser JavaScript belongs in any other service.

Frontend is internal. It is not published to the host and never talks to Crawler or any other backend service; it only produces the page. The browser reaches it through Gateway, which proxies page routes here and answers RPC paths itself.

## Why It Sits Behind Gateway

The browser must load the page and call RPCs from a single origin, otherwise every Connect call pays a CORS preflight — Connect sends `connect-protocol-version`, a custom header, so no call qualifies as a simple request.

Keeping Gateway as the browser's only way in preserves that single origin and keeps one external entry point for the system. The cost is one internal HTTP hop for page loads, which are rare compared to RPCs.

## Pages

| Route | Purpose |
|---|---|
| `GET /` | Crawler console — start a crawl and watch job status |
| `GET /health` | Plain-HTTP liveness for Compose |

## Assets

`client/index.html` is one static file with no build step, compiled into the binary with `include_str!`. A single self-contained file means the release image is a binary and nothing else, with no asset paths to get wrong at runtime.

`include_str!` registers the file as a rebuild dependency, so editing markup under `docker compose up --watch` rebuilds the image the same way editing Rust does. Once the UI outgrows one file, swap the handler for `ServeDir` and copy `client/` into the image.

## Talking to the Backend

The page calls the same Connect endpoints the gateway exposes, at relative paths, so it inherits whatever origin served it:

```javascript
fetch("/crawler.v1.CrawlerService/StartCrawl", {
  method: "POST",
  headers: { "content-type": "application/json", "connect-protocol-version": "1" },
  body: JSON.stringify({ baseUrl }),
});
```

Never hardcode a host or port. Relative paths are what keep the single-origin property true in dev, in Compose, and behind a real domain later.

Field names use protobuf canonical JSON, so `base_url` in the proto is `baseUrl` on the wire and enums are their value names, such as `CRAWL_STATUS_QUEUED`.
