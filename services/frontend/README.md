# Frontend

Static browser UI for inspecting Knowledge Base search and starting one-off crawls.

Frontend is not a runtime server or Cargo workspace member. Its Compose container copies `client/` into a shared volume and exits; Gateway mounts that volume read-only and serves the files. Browser requests and RPC calls therefore share one origin.

## Pages

| Route | Purpose |
|---|---|
| `/` | Search Knowledge Base with an optional page-type filter. |
| `/sources` | Start a crawl and poll its job status. |
| `/login` | Create a Gateway session with the shared password. |
| `/chat` | Send turns, watch topics run, and read the transcript with per-line timing. |

All page and client-side code belongs in `client/`. Backend calls use relative Connect paths such as `/crawler.v1.CrawlerService/StartCrawl`; do not hardcode a host or call internal services directly.

There is no bundler: the current build is a file copy. Compose Watch reruns it when a client file changes. Gateway applies `Cache-Control: no-store` to page responses so authentication is checked on every load.
