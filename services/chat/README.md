# Chat

Session and topic orchestration over [Engine](../engine). A **session** is one user's whole
conversation; a **topic** is one strand inside it, backed by one Engine execution; a **turn** is one
user message, which may reach several topics at once. Chat owns the routing, the focus pointer, the
per-session concurrency cap and the event log the client rebuilds itself from; Engine owns
everything about actually running a graph.

## Topics Are Produced, Never Created

There is deliberately no "new topic" control. The user creates **sessions**, and every message they
type is routed by one `SystemOneService.Decide` call (`intent.rs`, `TopicIntent::route`) carrying
three typed questions about the new message, answered together in a single round trip because a
second question on the same call is far cheaper than a second one:

- **`route`** — asked only once the session already holds topics: which existing topic, if any, the
  message continues, or whether it opens a new one instead. Read **first**, and that ordering is
  load-bearing, not incidental: putting `route` ahead of `actionable` means a bare fragment that
  plainly continues a live topic — "and?", "i'm still waiting" — is pinned to that topic before
  anything asks whether it carries a request of its own, so it is never interrogated for one. Below
  `ROUTE_CONFIDENCE_THRESHOLD` (0.5) the model cannot separate the options at all, and the
  deterministic fallback below is a better answer than its guess.
- **`actionable`** — does the message, alone, carry a request to act on. Below
  `ACTIONABLE_THRESHOLD` (0.35) the turn is clarified instead of acted on; at or above it, it
  proceeds. Getting this wrong is not symmetric: asking a person to repeat themselves costs one
  extra exchange, while letting an agent execution run a full minute on a bare greeting costs the
  whole exchange — so the turn only stops short once the model is fairly confident (under 35%) that
  it carries no request at all, rather than clarifying at the first sign of doubt.
- **`separate_themes`** — does the message raise more than one independent theme. Above
  `SEPARATE_THEMES_THRESHOLD` (0.75) it is split into one topic per theme. Splitting a message that
  actually holds a single theme costs a completion and leaves two half-topics behind, so this one
  asks for real certainty.

`separate_themes` is also the **only** branch that still spends an `llm-router` `Complete` call.
Continuing a topic, opening one fresh topic from the message verbatim, and clarifying are all read
straight off the typed `Decide` answer at zero extra cost — an approval needs no prose. Only a
confident split needs the model to actually write out each theme's own `title` and `question`:

```json
{"actions": [{"kind": "new", "title": "…", "question": "…"},
             {"kind": "new", "title": "…", "question": "…"}]}
```

The parse is forgiving on purpose — a fenced block, a bare object, or an object with commentary
after it all have to work — because a low-tier model does not reliably emit only JSON, and `split` is
the one path here still asking one to.

**Classification is advisory, never load-bearing.** Every failure mode — an unreachable decider, a
call that returns no answers, a `route` naming a topic id that is not in the session, a `split` that
fails to parse — falls back to continuing the focused topic, or to opening one topic from the
message itself when the session is empty. That fallback (`intent::fallback`) is the deterministic
behaviour the service had before any decision existed, so a turn is never lost to a bad or missing
classification.

**A clarification is the single load-bearing outcome.** When `actionable` comes back low and nothing
routed, `send_turn` publishes a session-wide `CLARIFICATION_NEEDED` event and returns: no topic is
created and no Engine execution starts. Critically, **no message row is written** for it either.
`already_delivered` recognizes a turn as delivered by finding a message row keyed on its `turn_id`,
and a clarified turn leaves none — so a client retry that reuses the same `turn_id` runs the routing
decision again rather than replaying a stale answer. That is the same idempotence every other turn
gets from its recorded message, produced here by recording nothing at all.

`question` is not decoration. It is the self-contained restatement of what the topic is about, and
it is the **only** state key the agent graph's prompt actually reads — `engine-core`'s prompt
builder takes `state.question`, and the tool executor's fixed-slug path searches the same key. A
follow-up parked under any other key would be silently ignored. `title` is the short label the panel
draws and is capped at `MAX_TITLE_CHARS`, the width of `chat.topics.title`.

## Focus and the Queue

Focus is a pointer on the **session** row (`focus_topic_id`), not a property of a topic. It moves on
exactly two occasions:

| Event | Focus |
|---|---|
| First topic in a session | Becomes focus |
| A turn *creates* a topic (new theme, or a continuation of a completed one) | Moves to it |
| A turn merely continues running topics | Unchanged |
| The **focused** topic **completes** | Moves to the next unfinished topic, oldest first; cleared if none remain |
| A topic **fails** or is **cancelled** | Unchanged |

A turn aimed at a finished topic is refused with a clear message rather than misrouted, and it is
refused before any Engine call is made — `execution_id` stays set forever once a topic reaches a
terminal status, so its presence says nothing about whether an execution is still there. Only
`status` does.

A reply to a **`Completed`** topic is not a refusal: it spawns a **child** topic carrying the parent's
answer forward as the child's `question`, so the past is not rewritten. The child goes through
`create_topic` like any other topic, so it is subject to the same concurrency cap, the same `Queued`
admission, the same session lock and the same `topic_started` / `topic_queued` events the client
already knows how to render.

At most `MAX_CONCURRENT_TOPICS` topics run at once per session. A topic created past the cap is
admitted as `Queued`, keeping its original `input_json` verbatim so that `promote_next_queued` can
start it with the intent it was created with rather than with empty state. That column defaults to
`{}` and never to `""`: Engine rejects an initial state that is not a JSON object, and a `Queued`
row carrying `""` would fail every promotion attempt while remaining the oldest queued row — wedging
the session's queue permanently, because a failed promotion is only logged.

## One Lock Per Session

`SELECT … FROM chat.sessions WHERE id = $1 FOR UPDATE` is the single serialization point for
everything that changes how many topics are `Running`, or where focus points:

- `create_topic` — the `COUNT(Running)` slot check and the `INSERT` are otherwise two unsynchronized
  statements, and concurrent creates could all pass the check before any of them committed.
- `finish_topic` — marking a topic terminal and moving focus off it are one atomic decision.
- `promote_next_queued` — picking the oldest `Queued` row and marking it `Running` are two
  statements, and two topics finishing at once would otherwise both pick the same row and start two
  Engine executions for one topic.
- `ResetSession` — so a wipe cannot race a concurrent create, finish or promotion.

Holding the lock across `start_execution` keeps a network call inside a transaction, which is a
smell; at this scale — one session, one short call — it is the right trade against a two-phase
design. `get_or_create_session` itself is *not* under that lock: it uses
`ON CONFLICT (user_id) DO NOTHING` followed by a SELECT, because the chat page opens `GetSession`
and `StreamEvents` in parallel and, right after a `ResetSession`, neither finds a row. A plain
INSERT made the loser fail with a unique-constraint violation the user saw as a 500.

Topic listings are ordered `created_at` explicitly. Without an `ORDER BY` this is heap order, which
Postgres reshuffles as rows are updated — and topic rows are updated on every status change. The
client uses that order both to draw the panel and to pick the newest topic when the server has
cleared focus.

## Turn Dedup

`SendTurn` is idempotent on `turn_id`, and the deliver span runs inside one advisory-locked
transaction, so a client retry observes the turn as either fully delivered or not delivered at all.
**Classification happens before that transaction opens.** The llm-router call is a read: it decides
what to deliver, it changes nothing, and a re-run of it costs only tokens — so holding a pooled
connection idle-in-transaction across a call with a 30-second deadline buys nothing and starves
every other request of the pool. The delivery then re-reads the session view under the lock, and an
action naming a topic that vanished in between is simply dropped. Two further properties of the
design are easy to get wrong:

**The marker is keyed on the turn across the whole session**, never on `(focus topic, turn_id)`.
Focus is not stable between a delivery and the retry: a continuation writes its marker on the newly
created *child*, and by the time a retry arrives focus may have moved on again. Keyed on the focus
topic, such a retry misses the marker and re-delivers — a second child topic and a second Engine
execution for one user message, or worse, the follow-up `Interrupt`ed into whatever unrelated topic
happens to hold focus now.

**The marker is written after delivery, not before.** The check stays up front so a retry after a
successful delivery short-circuits without interrupting the execution a second time, but the write
happens only once the turn has actually landed. The other order loses the turn for good on a
transient `Interrupt` failure: the documented same-`turn_id` retry would find the row and report
success as a no-op.

The `already_delivered` check runs twice — once before classifying, so a retry of a delivered turn
never pays for an llm-router call, and once under the lock, where it is the guarantee.

The advisory lock is keyed on the **turn**, not the session, because the span opens nested
transactions of its own (`insert_topic`, `promote_next_queued`) that take the session row lock on
other connections — taking that same lock here would deadlock against them. Two different turns stay
concurrent either way. `pg_advisory_xact_lock` takes one `bigint`, so the uuid's high half is used;
a collision costs two unrelated turns a moment of serialization and nothing more.

## The Engine Watch Loop

Every topic that reaches `Running` gets a background task consuming its Engine event stream —
spawned from `create_topic`, from `promote_next_queued`, and from `recover()` at startup, which
re-attaches a watcher to every topic this process left `Running` before a restart. Recovery works
only because Engine's `StreamEvents(execution_id)` replays an execution's **whole history from
version 1** on every subscribe rather than only new events; if that ever changes, `recover` needs a
version cursor too.

A stream that ends or errors *without* a terminal event — Engine restarted mid-execution, or the
streaming deadline was hit — used to strand the topic at `Running` forever. The watcher now
reconnects: it waits out an exponential backoff from `RECONNECT_BASE_DELAY` capped at
`RECONNECT_MAX_DELAY`, calls `stream_events` again, and keeps going until a terminal event lands or
the topic is no longer `Running`. Exiting on "no longer `Running`" is what stops a watcher
reconnecting forever to a topic a reset or delete removed out from under it.

Chat's two Engine deadlines are deliberately far apart. A client timeout in connectrpc is a
*whole-call* deadline enforced on **every frame poll** of a streaming call, not just on connect, so
the unary `CALL_TIMEOUT` applied to `StreamEvents` cut the event stream of any execution running
longer than it — `finish_topic` never ran and the topic stuck at `Running`. There is no "no
deadline" setting (an unset per-call timeout falls back to the client's default), so
`LONGEST_WATCHABLE_EXECUTION` is a long finite bound instead, and the reconnect loop covers the rest.

Every Engine call carries the caller's `Principal` in `x-principal-*` headers, built from the
session row rather than from a request header — a background watcher has no request to read. Without
them Engine records `user_id = None` on every execution Chat starts and its ownership check can
never fire.

### Why Replay Dedup Is Keyed On The Event Id

Not on `version`. `engine_core::Execution::event()` currently stamps *every* event of an execution
with `version: 1`, so a `version`-based high-water mark treats the first event received as having
already covered every later one — the terminal event included — and a reconnect, or even the very
first pass, would silently drop it. `event.id` is a fresh UUID per real event and stays stable across
a replay (same row, same id), so it is the only key that works.

Each watcher remembers the last `MAX_REMEMBERED_EVENT_IDS` ids in a ring, oldest evicted first. A
watch can run for an hour over an execution that emits the whole time, and the ids only ever need to
cover what a reconnect replays ahead of the point the stream broke — an unbounded `HashSet` grew one
`String` per event for the life of the task.

That in-memory ring is the *task's* dedup. The durable one is the `UNIQUE (occurred_at, event_id)`
constraint on `chat.events` with `ON CONFLICT DO NOTHING`, which is what makes a replay idempotent
across restarts: before it existed, `recover()` re-attaching a watcher persisted Engine's full replay
a second time and permanently duplicated the log the next client to reload was handed. Engine's own
event id is a string, so it is hashed into the uuid column rather than parsed — same input, same row,
every replay.

## `chat.events`

Chat's own append-only log, and the reason a reloaded page comes back with everything it saw rather
than blank. `StreamEvents` replays this table's rows for the session and then chains the live tail
onto them.

The two halves are opened in the only order that loses nothing: **subscribe first, read the snapshot
second.** The other order leaves a window between the read and the subscribe in which a published
event reaches nobody — the bus is a plain broadcast channel with no history, so an event missed there
is missed for good, and a topic could finish without the client ever hearing. Subscribing first can
instead deliver an event twice. That is the right way round to be wrong: the client refreshes with
`GetSession` on a lifecycle event, so a duplicate is a redundant refresh while a gap is a permanently
stale panel. Publishing when nobody is listening is the normal state of a chat with no browser
attached, so the send's one error case is recorded rather than treated as a failure.

| Table | Class | Bound | Hot-path query |
|---|---|---|---|
| `sessions` | Reference data | One row per user | `get_or_create_session`; the `FOR UPDATE` lock |
| `topics` | Reference data | Grows with topic count, not time | Session view, oldest-queued poll, running count — all `session_id`-led, narrowed by `status`, ordered by `created_at` |
| `messages` | Reference data | One row per user turn, `UNIQUE (topic, turn)` | The session transcript, one query for every topic |
| `events` | Append-only log | Monthly partitions; `CHAT_EVENTS_RETENTION_MONTHS` (unset = forever) | `StreamEvents`' replay, `WHERE session_id = $1 ORDER BY id` |

`chat.events` is `PARTITION BY RANGE (occurred_at)` from its first version, because retention has to
be a `DROP TABLE` of one child and converting a populated table to a partitioned one later is a full
rewrite under lock. It therefore **cannot** live under `crate::entity::`, which
`get_schema_registry("chat::entity::*")` globs: schema-sync emits a plain `CREATE TABLE`, cannot
express a partitioned parent, and is not inert against one created by hand — the same collision
`services/engine/src/execution_event.rs` hit. The table, its indexes and its partitions are created
by literal statements in `event_log.rs`, one of gate-database's sanctioned exceptions, and every
value interpolated into them comes from `chrono` arithmetic or from a name that module itself
formatted, never from a request. The three sync-managed entities stay under `crate::entity::` and the
glob keeps covering them.

The ORM entity declares `id` as its single primary key. That is the ORM's row identity, not a claim
about the database constraint, which is `PRIMARY KEY (occurred_at, id)` — Postgres requires the
partition key in every unique constraint on a partitioned table. `id` is one `bigserial` sequence
shared by all partitions, so it stays globally monotonic in insertion order, which is what the replay
orders by.

**Partition maintenance runs daily, not only at startup.** `MONTHS_OPEN_AHEAD` months are kept open
beyond the current one, because a range with no partition makes the `INSERT` itself fail, and a
process that stays up across a month boundary would walk off the end. Inserts are clamped into that
open window for the same reason: Engine stamps `occurred_at` on its own clock and replays an
execution's whole history from version 1, so a restart across a month boundary would otherwise drop
every event it relays. Storing such an event at the edge of the window keeps it. Startup also asserts
that `chat.events` really is the partitioned parent, rather than a plain table from before
partitioning — against which `CREATE TABLE IF NOT EXISTS` is a silent no-op and the log would grow
with no retention path at all.

A failed event insert is logged, never propagated: losing the durable copy of a `topic_started` must
not fail the topic that just started, and the live event still goes out.

`ResetSession` deletes the session's events along with its topics and messages — a reset means "none
of this happened", and the replay a reconnecting client gets must not resurrect topics that no longer
exist. That is a per-session **domain** delete over one user's rows, not the log's retention path,
which is `DROP PARTITION`. Both carry an `occurred_at` lower bound so Postgres prunes every month
that cannot hold a row of the session. The `session_reset` event itself is published but deliberately
not persisted: the row would land in the log of a session that was just deleted, and the next
`get_or_create_session` mints a new session id anyway.

## Events On The Wire

There are two status spellings, not three. `chat.topics.status` is a smallint, `chat.v1.TopicStatus`
is the contract, and `status_proto` in `topic_status.rs` is the one exhaustive match between them —
so a status added to the column does not compile until the wire knows about it too. Wherever a
status needs a *word* — the turn-routing decision's state, a terminal `notification` — `status_word` takes the
proto enum's declared name and drops its `TOPIC_STATUS_` prefix. There is no hand-written table of
`"running"`, `"completed"`, `"cancelled"` anywhere, so none can be forgotten.

`TopicEventKind` is deliberately **not** collapsed the same way. It is Chat's domain vocabulary and
also the `chat.events.kind` column's, and a persisted spelling must not be hostage to the names in a
proto file; it has no `Unspecified` non-state to carry around either. It becomes
`chat.v1.ChatEventKind` in one exhaustive match in `main.rs`, and its column spelling is derived, not
written twice: the enum derives `Serialize`/`Deserialize` with `rename_all = "snake_case"`, and
`as_column`/`from_column` go through that one declaration, so a new variant is readable back the
moment it is writable.

On the wire a client therefore reads the enum's declared name (`CHAT_EVENT_KIND_TOPIC_STARTED`,
`TOPIC_STATUS_RUNNING`), never a string whose vocabulary lives in a comment.

`topic_created` is published **before** the event that says how the topic was admitted
(`topic_started` or `topic_queued`): the replay is the only thing a reloading client has, so the
event that says a topic exists — and what it is about — has to be in it, not just the status
transition. A terminal topic produces two events in the order a client must see them: the lifecycle
event, then the `notification` carrying the answer. The lifecycle event names the outcome it
actually is — `topic_completed`, `topic_failed` or `topic_cancelled`, one per terminal status —
because a cancellation is not a failure and a client that renders it in red is telling the user
something untrue about their own click.

An Engine event kind this build of Chat does not know arrives as `Unspecified` and is treated as
progress. A progress event's `step` is the Engine enum's declared name minus its
`EXECUTION_EVENT_KIND_` prefix, lowercased, so a kind added to `engine.v1` reaches the browser under
its own name rather than as `unspecified` through a forgotten match arm. All of that conversion
happens in `engine_client.rs`, the only file that talks to Engine, so a change to
`engine.v1.ExecutionEvent` stops there.

`EventBus` is one process-wide broadcast channel carrying every user's events; `StreamEvents`
filters the live tail down to the requesting session on the server rather than leaving it to the
client.

## The Chat Page

[`services/frontend/client/chat.html`](../frontend/client/chat.html) is the only client, and it is
written against the two halves above: `GetSession` is the snapshot, `StreamEvents` is the tail.

**One transcript for the whole session.** Every topic's turns and answers are interleaved in time and
each line is tagged with the topic it belongs to — the Claude Code model, where background work
reports back into the one conversation rather than into a pane of its own. The tag always names the
**root** conversation, never a child's own synthesized title: a child topic is more turns in the same
thread, so its label must not flip to whatever the routing decision called the follow-up.

**Ordering is `(at, sequence)` and nothing else.** A user turn sits at its message's `created_at`; a
topic's acknowledgement sits at its *first* message's `created_at`, so it lands right behind the turn
that opened it; a finished topic's answer sits at the topic's `updated_at`, which is when it actually
arrived. `sequence` breaks the tie those three share by construction — turn, then acknowledgement,
then answer. No synthetic millisecond offsets and no proximity window: both were compensating for
anchoring the acknowledgement to `topics.created_at`, which is stamped *before* the message row the
turn produced.

**Lifecycle events reload; everything else appends.** `topic_created`, `topic_started`,
`topic_queued`, the three terminal kinds and `focus_changed` all change what `GetSession` would say,
so they trigger one debounced `GetSession` and a full re-render — a reconnect replays dozens of them
in a burst and they all want the same snapshot. Progress and notifications are never part of that
snapshot, so they are appended in place.

**Rendering an event must never throw out of the loop.** `StreamEvents` replays the session's entire
persisted history on every reconnect, so an uncaught error would die on the same event every time the
client reconnects, freezing the transcript forever — every later lifecycle event, including the one
that finishes a running topic, never gets a chance to run. Failures go to the debug panel instead,
which logs every RPC, every event and every connection change, independent of what the transcript
chooses to draw.

**The Connect streaming envelope is read by hand:** one flag byte (`0x02` = end-stream) and four
big-endian length bytes, then that many bytes of JSON; the single request message of a
server-streaming call is itself one such envelope, not bare JSON. The length header comes from the
network, so a payload claiming more than `MAX_ENVELOPE_PAYLOAD_BYTES` fails the stream rather than
growing the read buffer to whatever it asked for; the retry loop then reconnects with backoff.

Answers are rendered by a small markdown pass that escapes HTML **first** and only then converts a
subset of markdown to tags, so model output can never inject markup. There are no external libraries
because the CSP allows none. The layout is exactly one viewport tall and never scrolls as a document:
each pane owns its own scrollbar, which requires `min-height: 0` on every ancestor of a scrolling
pane — a flex item's default `min-height: auto` refuses to shrink below its content and would push
the page past `100vh`. Clicking a topic row moves the server's focus and highlights it; the
transcript is session-wide and does not change. When the last unfinished topic completes the server
clears focus, so the page falls back to the newest topic for the highlight only — the routing
decision, not that pointer, decides where the next turn lands.

## Input Limits

Checked once, at the RPC entry, so everything behind it takes values it can trust:

| Value | Limit | Why here |
|---|---|---|
| `CreateTopicRequest.input_json` | `MAX_INPUT_JSON_BYTES`, and must be a JSON **object** | Merged into an Engine execution's initial state, which rejects a non-object |
| `SendTurnRequest.content` | `MAX_CONTENT_CHARS` | Long enough for a pasted stack trace, short enough that a runaway client cannot push an unbounded string through the routing decision and into `chat.messages` |
| `CreateTopicRequest.title` | `MAX_TITLE_CHARS` | The width of `chat.topics.title`; past it, an `invalid_argument` naming the limit rather than a 500 out of Postgres |

Each of those is also the width of the column behind it, from the same constant: `MAX_CONTENT_CHARS`
is declared on the `message` entity and spent both by `bounded_content` and by the column type, so
the entry check and the schema cannot drift apart. An Engine answer is capped the same way, at
`MAX_RESULT_SUMMARY_CHARS`, where it enters `finish_topic` — otherwise the only bound on a
`chat.topics.result_summary` row is however much the model felt like writing.

Errors never carry internals outward. `ChatError`'s detail — SeaORM's rendering of a Postgres error,
or Engine's own message — names tables, columns and constraints, so it goes to the log and a fixed
internal message goes onto the wire.

## Config

`DATABASE_URL`, `ENGINE_URL`, `LLM_ROUTER_URL`, and the optional `CHAT_EVENTS_RETENTION_MONTHS`
(unset keeps every partition, which is the default).
