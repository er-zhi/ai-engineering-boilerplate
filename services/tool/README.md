# Tool Service

The registry and executor for everything an agent can *do*. Engine's Dispatcher asks for the
catalog, injects it into the model's prompt, and calls `Execute` when the model emits a tool call.

## Capabilities, Not Topics

Tools are verbs, never subjects. `web_search`, `web_fetch`, `kb_search` and `kb_read_document` are
the four system tools, and that list does not grow when a new subject area comes up — a `weather` or
`stock_quote` tool would be `web_fetch` with a topic glued on, answering one question and leaving
every neighbouring one unanswered. Nothing here matches on what the user is asking about, and no
tool description names a vendor: `web_search` is described by what it returns, not by which backend
happens to be configured.

`web_fetch` takes several URLs, fetches them concurrently, and returns the one carrying the most
readable text. Public pages holding the same fact are individually unreliable — one 403s a
datacentre IP, another is a JavaScript shell that extracts to nothing, a third is slow — so an agent
handed a single URL spends a whole turn per failure. Fetching the candidates together costs one turn
whatever any single source does, and a total failure returns one error listing every reason, so the
model can tell "try other pages" from "this needs a different search".

Returning the fullest page rather than the fastest is deliberate. Whoever answers first is decided
by network latency, which has nothing to do with whether the page holds the answer: a marketing
stub served from a CDN beats the page with the data on it every time. The caller asked for something
to answer from, so that is what the race is decided on.

It does not wait for all of them either. As soon as `ANSWERS_BEFORE_CHOOSING` pages have come back
it stops and picks the fuller of those, so one slow or hanging host cannot hold up an answer two
other sources already gave — the comparison needs something to compare against, not every candidate.
The rest are dropped mid-flight.

A fetch whose extraction yields less than `MIN_READABLE_TEXT_CHARS` fails instead of succeeding
quietly. The threshold is deliberately low — under a single sentence — because that is the honest
limit of what a content check can know: a page shorter than that carried no article at all (a
script shell, a paywall stub, a redirect notice), while anything longer is prose the model must
judge for itself. It is not, and cannot be, a check that the page answers the question; nothing here
knows what was asked. What stops a thin page from becoming a confident wrong answer is the rule in
Engine's prompt that every value stated must appear in a tool result.

## Risk Policy

`risk` is decided by deterministic Rust, never by a model:

| Risk | Execute does |
|---|---|
| `ReadOnly` | Runs it. |
| `Write`, `Destructive` | Returns `RequiresApproval` without running it. |

The approval flow itself does not exist yet — Engine has no way to carry a human decision back into
a running execution. Returning `RequiresApproval` is the honest answer until it does: the alternative
is running an unreviewed write, or hanging.

## System Tools and Seeding

The four system tools are seeded at startup with `user_id = NULL` and activated without the LLM
review a user-created tool goes through — they are pre-vetted code paths in this binary, not
submitted definitions. Their slugs are reserved: `create_tool` rejects a user row claiming one.
`list_tools` publishes only `Active` rows, and that catalog is what Engine injects into the
tool-calling prompt.

## SSRF Boundary

`ensure_public_url` is applied by the RPC entry points, deliberately **not** inside `fetch()`. The
untrusted input is the model's URL, so the check belongs where that input arrives — the same place
`services/crawler/src/main.rs` applies `validate_base_url`. Keeping it out of `fetch()` also lets
`fetch()`'s own tests talk to a loopback server. Every candidate URL is checked before any request
goes out, not one at a time as they are tried.

## Search Providers

One trait, two implementations, chosen by which key is configured — You.com first, then Brave, then
a clear "not configured" error. The working You.com endpoint is
`GET https://ydc-index.io/v1/search?query=<q>` with an `X-API-Key` header, returning
`{"results": {"web": [{url, title, description}]}}`. Note the host: the `api.ydc-index.io`
subdomain returns 403. You.com documents no result-count parameter, so the limit is enforced
client-side.

`YOU_SEARCH_API_KEY` and `BRAVE_SEARCH_API_KEY` sit in this service's own environment. That is
temporary: credentials move to the Integrations Service once it exists, and no service should hold
a provider key directly.

## Schema (`tool`)

`tool.tools` is reference data — it grows with the number of tools, not with time — so it is not
partitioned and is not swept.

Uniqueness is "one slug per owner, with system tools sharing the NULL owner", which needs
`COALESCE(user_id, <zero uuid>)`. Schema-sync cannot express an expression index, so it is created
manually after sync. Postgres only matches an expression index against the same expression, so the
per-execute lookup — which filters the bare columns — gets a plain `(user_id, slug)` index of its
own.

That manual index collides with schema-sync on **sea-orm 2.0.3**: sync always tries to drop a unique
index it did not create, through `ALTER TABLE ... DROP CONSTRAINT`, which fails because the index is
not a constraint. Startup tolerates that specific error and recreates the index. Revisit on upgrade.

A uniqueness rule the code depends on must use the **field-level** `#[sea_orm(unique_key = "...")]`
attribute. The struct-level form with a `columns` list only generates a `find_by_*` accessor and
emits no index at all, so the constraint silently does not exist.

## Configuration

- `DATABASE_URL`
- `KNOWLEDGE_BASE_URL`, `LLM_ROUTER_URL`
- `YOU_SEARCH_API_KEY` or `BRAVE_SEARCH_API_KEY` (optional; without either, `web_search` returns a
  "not configured" error rather than failing at startup)

Every outbound HTTP call is bounded: one client is built with a default timeout in one place and
injected, and `Execute` narrows it to the tool row's own `timeout_seconds`.
