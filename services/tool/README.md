# Tool Service

The registry and executor for everything an agent can *do*. Engine's Dispatcher asks for the
catalog, injects it into the model's prompt, and calls `Execute` when the model emits a tool call.

## Capabilities, Not Topics

Tools are verbs, never subjects. `web_search`, `web_fetch`, `kb_search` and `kb_read_document` are
the four system tools compiled into this binary, and that list does not grow when a new subject area
comes up. Nothing here matches on what the user is asking about, and no tool description names a
vendor: `web_search` is described by what it returns, not by which backend happens to be configured.

A live external fact still has to come from somewhere, and this service draws a hard line around
where that "somewhere" gets decided. The code supplies capabilities — a fetch, a race, an executor
for a row of endpoints — and a subject reaches the system only as a registry row an **operator**
writes into a file outside the repository (`DECLARATIVE_TOOLS_PATH`, see "Declarative Tools" below).
A fresh clone therefore ships the executor and no subjects at all: that is the intended state, not a
step someone forgot. `CreateTool`, the RPC a user or an agent calls to define a new tool, does not
accept a `sources` field — only an operator writing to that file can wire up a race of endpoints; a
tool submission never can. Letting arbitrary outbound HTTP be driven by end-user input is its own
SSRF and quota decision, a different and larger one than an operator vetting a file before it ever
reaches the container.

`web_fetch` takes several URLs and races them through the shared `race` primitive (below), returning
the one carrying the most readable text. Public pages holding the same fact are individually
unreliable — one 403s a datacentre IP, another is a JavaScript shell that extracts to nothing, a
third is slow — so an agent handed a single URL spends a whole turn per failure. Fetching the
candidates together costs one turn whatever any single source does, and a total failure returns one
error listing every reason, so the model can tell "try other pages" from "this needs a different
search".

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

That covers the first hop. A redirect is a second request to an address nothing has checked yet, so
`get_guarded` (`tools/web_fetch.rs`) follows redirects by hand and re-applies `ensure_public_url` to
every `Location` before following it, rather than trusting an automatic redirect to stay inside the
guard. `bounded_http_client` builds the no-redirect client behind exactly this, with `Policy::none()`
so reqwest hands each hop back instead of following it — under the default policy it would already
have followed the hop before this code ever saw the target. `fetch()`, `fetch_richest()`, and a
declarative source's request (`declarative::ask`) all go through `get_guarded` on this client, so a
public page that answers with a private `Location` is refused at the hop it appears on, not fetched
one request later.

That guard only applies to URLs the model chose. A search vendor's own redirects (an http→https
upgrade, a moved path, a CDN hop) are not requests to an address this process picked, so there is
nothing for the guard to re-check hop by hop — `redirect_following_http_client` builds the ordinary,
redirect-following client the search providers use instead, so a vendor's routine 301/308 does not
fail the whole tool call.

A search *hit's* `url` is a different matter again: it is data the vendor returned, chosen by
whatever page ranked well for the query, not a request this process picked either. `web_search`'s
own prefetch (`tools/web_search.rs`) treats it exactly like a caller's `web_fetch` url — checked by
`ensure_public_url` before any request for it goes out — because it is just as untrusted as one a
caller sent directly; see "A search returns the pages it found" below.

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

### A search returns the pages it found

`run_web_search` does not just relay a provider's `{title, url, snippet}` triples any more. Once
the provider answers, `tools/web_search.rs`'s `attach_prefetched_text` races a fetch of its top
`PREFETCH_CANDIDATES` hits (four, through `race` below, `take = 2`, a ~1 s per-operation cap) and
attaches whichever succeed as a `text` field. The reason is measured, not assumed: 26% of tool
turns took the shape search → fetch → answer, and the middle round was a generative call whose
entire output was "fetch these URLs" — URLs the search had already returned a round earlier
(`docs/superpowers/plans/2026-09-18-latency-round-two.md`). Attaching the readable text up front
lets the model answer straight from the search result instead of spending a further round asking
for the same pages back by name — the dependency between "search" and "read what it found" is
real, but the round trip through the model to say so is not.

A hit that fails `ensure_public_url`, loses the race, or was not among the top candidates keeps its
snippet and gains nothing: prefetching only ever *adds* a `text` field, so it can never turn a
search into an error — a slow or hostile page degrades to exactly what `web_search` returned before
this existed. Engine's rendering cap (`MAX_TOOL_RESULT_CHARS` in
`services/engine/src/executors/llm.rs`) was raised alongside this change, applied to the whole
rendered tool result rather than per hit, so the attached pages actually reach the model instead of
being cut before the model ever sees them.

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
- `DECLARATIVE_TOOLS_PATH` (optional; unset, the service starts with no declarative tools loaded —
  see "Declarative Tools" below for the row shape and the Compose override that mounts it)

Every outbound HTTP call is bounded: one client is built with a default timeout in one place and
injected, and `Execute` narrows it to the tool row's own `timeout_seconds`.

## `race`

`race` (`src/race.rs`) is the primitive `web_fetch`, a search's own prefetch, and Declarative Tools
all run on: hand it a slice of operation *factories*, and it starts `fan_out` of them at once, keeps
the first `take` that succeed, and drops whatever is still running the moment that quorum is
reached. It knows nothing
about HTTP, URLs, or what any operation is about — it is the shape of "several ways to get the same
thing," reused wherever that shape shows up.

Operations are factories, not futures. A future that has already resolved — or been cancelled at its
own timeout — cannot be polled a second time, so starting the next candidate once a slot frees up
needs a fresh attempt, not the same value reused. `fan_out` is the width: how many operations run at
once, with the rest waiting as reserve and starting only when a running one finishes, fails, or times
out. `take` is the quorum: how many successes are enough. Reaching it drops the whole set of
in-flight futures at once, which cancels them where they stand rather than waiting them out — a slow
or hanging operation never holds up an answer the others already gave.

`race` returns an `Outcome`, not a `Result`: fewer than `take` successes, with the rest genuinely
failed, is a normal outcome a caller reasons about, not an error that unwinds. `taken` carries up to
`take` `(name, value)` pairs in arrival order; `failures` names every operation that ran and did not
succeed, one `"name: reason"` per attempt — an operation still in flight when the quorum was reached
is cancelled and has no verdict, so it appears in neither list. The name a caller gives an operation
is a borrowed `&str`, carried through to `Outcome` and never owned or leaked, because both
`fetch_richest` and a declarative row's `run` build one of these fresh per execution.

## Declarative Tools

`declarative_seed` loads tool rows from a file an operator writes, on top of the four system tools
above. It ships no subject of its own — see "Capabilities, Not Topics" — so a fresh clone has the
executor and no declarative tools until an operator supplies `DECLARATIVE_TOOLS_PATH`.

Setting `DECLARATIVE_TOOLS_PATH` is optional; unset, the service starts normally with none loaded.
When set, the file is read once at startup and every row is upserted as a system row (`user_id =
NULL`, `status = Active`, `risk = Risk::ReadOnly` — a declarative row only ever issues GETs, so risk
is decided by this code, never read from the file), skipping the LLM review a user-submitted tool
goes through, the same way the compiled-in system tools do. **One bad row refuses the whole file**:
a half-loaded registry is worse than an empty one, so a parse or validation failure on any row is
logged and none of the file is loaded, rather than loading the rows that did parse.

Each row is `{slug, name, description, timeout_seconds, input_schema, sources}`; `sources` is the
shape `crate::tools::declarative::parse_sources` checks — `fan_out`, `take`, and a list of
`{name, url, pick}` sources. `{field}` in a `url` is filled from the tool's input, percent-encoded,
and only from fields `input_schema` declares — a placeholder naming anything else is refused when
the file is read, not on a caller's first request. `pick` is a dotted path into the source's JSON
reply (`"current.value"` reaches into `{"current": {"value": …}}`), and it takes the same
placeholders under the same check, because a reply's shape often depends on what was asked for: a
source told to report one named thing commonly keys the answer by that name, so `"rates.{to}"`
reaches into `{"rates": {"EUR": …}}` when `to` is `EUR`. A `pick` is filled **verbatim** where a
`url` is percent-encoded — it addresses JSON, not a host, and a key with a space in it would
otherwise be sought as `%20` and never found. A slug already reserved for a
system tool (`slugs::RESERVED_FOR_SYSTEM_TOOLS`) or repeated within the file is refused.

A minimal row, with a placeholder subject and endpoints that do not exist:

```json
{
  "slug": "example_reading",
  "name": "Example reading",
  "description": "Returns a reading from independent providers at once.",
  "timeout_seconds": 8,
  "input_schema": {
    "type": "object",
    "required": ["place"],
    "properties": {"place": {"type": "string"}}
  },
  "sources": {
    "fan_out": 2,
    "take": 2,
    "sources": [
      {"name": "alpha", "url": "https://alpha.example.com/?q={place}", "pick": "reading.value"},
      {"name": "beta",  "url": "https://beta.example.com/{place}",     "pick": "value"}
    ]
  }
}
```

Every source races through the same `race` primitive `web_fetch` uses, but a declarative row's
answer looks different from `web_fetch`'s: two sources that both answer come back as **two**
entries — `{"source": "alpha", "value": …}` next to `{"source": "beta", "value": …}` — never one
chosen value. Agreeing sources confirm each other, and disagreeing ones are a fact the model has to
see, so nothing here averages or picks between them; that judgement belongs to whoever reads the
result, not to this code.

`compose.yaml` intentionally does **not** mount a declarative-tools file: Compose creates an empty
*directory* at the bind-mount source when the host path does not exist, which every fresh clone
hits by default since the file is gitignored and operator-supplied. Rather than ship a mount that
litters the repo root with a stray directory (or commit a fake file just to keep it happy), an
operator who wants declarative tools adds the mount and the env var at `up` time with an inline
override, without creating a second compose file:

```bash
# Write declarative-tools.json in the repo root first (rows shaped as above), then:
docker compose -f compose.yaml -f - up tool <<'EOF'
services:
  tool:
    environment:
      DECLARATIVE_TOOLS_PATH: /etc/tool/declarative-tools.json
    volumes:
      - ./declarative-tools.json:/etc/tool/declarative-tools.json:ro
EOF
```

`declarative-tools.json` is gitignored: it is an ops artifact, never a repository file.
