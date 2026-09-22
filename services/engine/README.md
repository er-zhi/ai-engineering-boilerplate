# Engine

Runs agent graphs. A **graph** is a declarative set of nodes and edges; an **execution** is one run of a graph, advanced one super-step at a time by the tick loop until it completes, fails, or parks on a wait.

The pure domain lives in [`engine-core`](../../engine-core) — `Graph`, `Execution`, `step()`, the reducers and the conditions, all I/O-free and all synchronous, and so unit-testable with no Postgres and no running service. This service is everything around it: Postgres, leases, the tick loop, the cron scheduler, the event stream, and the two outbound calls a node can make (LLM Router for `llm` nodes, Tool Service for `tool` nodes).

The design this is built to is [`docs/superpowers/specs/2026-09-15-engine-design.md`](../../docs/superpowers/specs/2026-09-15-engine-design.md).

## Capabilities, Not Topics

The prompts this service builds describe **behaviour**, never a subject area. `executors/llm.rs` tells the model to call a tool before answering what it is not certain of, to keep going until it has the actual value, and to answer tersely because the reply is read aloud. It never names a subject, a tool, or a route.

The only place a tool is named is the catalog the `Dispatcher` fills in from Tool Service's live `ListTools`. The model aims itself with that catalog; the code does not aim it. The catalog travels as `LlmNodeConfig.available_tools`, a `Vec<CatalogEntry>` the dispatcher fills and the executor reads back through the same type — the JSON key and the entry's own fields are the serde form of one declaration, never a `json!` literal on the writing side and a `#[serde(default)]` field on the reading side, which would answer a rename with an empty catalog and a model that quietly stops calling tools.

The same rule governs the guards around a generative call. A reply is judged against the material it was written from, and a composed argument against the message it came from — both by typed decisions, never by reading the wording of the reply, because a narration, a refusal and a confident guess are indistinguishable as text and impossible to enumerate across languages. What each guard is calibrated against is in [docs/decision-calibration.md](../../docs/decision-calibration.md).

`engine-core` holds no other service's catalog either: `rag_graph` takes the knowledge-search tool slug as a parameter, and [`builtin_graphs.rs`](src/builtin_graphs.rs) supplies it.

## Built-in Graphs Are Registered Idempotently

`builtin_graphs::register_all` runs on every boot and calls `Service::register_graph_if_changed`, which skips the insert when the newest stored version already holds exactly that definition. A restart that changed nothing adds no row. `RegisterGraph` over RPC keeps the other semantics — every call is a new version — because a client re-registering is stating intent, not repeating a boot.

## The Graph JSON Contract

`RegisterGraph.definition_json` is **Engine's** contract, not a window onto any Rust type. Graphs are dynamic JSON by design — a client composes one at runtime — so it stays opaque text in the proto and is specified here.

A definition is one JSON object:

| Field | Type | Required | Meaning |
|---|---|---|---|
| `id` | string | yes | The graph slug. `RegisterGraph.graph_id` is what the row is keyed by; this field travels with the definition. |
| `version` | integer ≥ 0 | yes | Informational. The stored version is assigned by Engine, one higher than the newest row for the slug. |
| `user_id` | UUID or `null` | yes | The owner, or `null` for a graph everyone may run. |
| `entry` | string | yes | The id of the node an execution starts on. |
| `nodes` | array of node objects | yes | See below. |
| `edges` | array of `{from, to, condition}` | yes | `from`/`to` are node ids. |
| `answer_pointer` | string or `null` | no (defaults to `null`) | An RFC 6901 JSON Pointer into final state naming **where this graph's answer lives**. |

Every node object carries `"type"` plus that type's own fields: `Task {id, kind, config}`, `FanOut {id, source, item_var, target}`, `FanIn {id, key, reducer}` (`reducer` ∈ `Replace`/`Append`/`Merge`), `Subgraph {id, graph_id, version, input, output_key}`, `Wait {id, on}` (`on` ∈ `{"Approval":{"risk":…}}`, `{"ExternalEvent":{"key":…}}`, `"UserInput"`, `{"Timer":{"until":…}}`), `End {id}`. A `condition` is `"Always"`, `"Failed"`, `{"Truthy":pointer}`, `{"Exists":pointer}`, `{"Eq":[pointer, value]}`, `{"Not":condition}`, `{"And":[…]}` or `{"Or":[…]}` — a closed set over JSON Pointers, never a scripting language.

Three of those node fields are JSON Pointers into state rather than node ids, which their names do not say on their own: `FanOut.source` points at the array being fanned out over, `FanIn.key` names the state key the folded array of branch results is written to, and `Subgraph.input` points at the value passed as the child execution's input. On a `Task`, `config["state_key"]` names where that node's output lands and `config["reducer"]` how; **absent means the output never touches state at all** and only drives `Failed`/`Always` edge conditions. No other node kind takes that path.

`Condition::Failed` is never evaluated alongside the others: `evaluate_condition` only ever sees the state a node produced, never whether it errored, so it returns `false` for `Failed` and `step()` special-cases the failed-node path before consulting conditions at all.

`validate()` runs once, at registration, and reports **every** problem it finds rather than the first: the `entry` node must be declared, every edge endpoint must be a declared node, and every `FanOut` target must lead to exactly one `FanIn`. A definition that fails is `InvalidGraph`; a definition that does not parse is the same error with the parse message.

### A Graph Declares Where Its Answer Lives

`answer_pointer` is the whole reason a consumer never has to know a graph's internals. `engine_core::step()` resolves it against final state at the one moment the graph and the final state are both in hand — when the execution completes — and the resolved value travels on the completion event as `result`. A graph that declares no pointer, or whose pointer resolves to nothing, simply has no answer; that is not an error, and the event still carries `final_state`.

All three built-in graphs declare `/llm/reply` (`engine_core::llm_reply_pointer()`): each funnels through an `llm` node whose `state_key` is `llm`, and llm-router's answer is parsed into `engine_core::LlmOutput` under that key, so `reply` is the answer whichever of them ran.

## What `StartExecution` Is Given

`StartExecutionRequest.input_json` stays opaque text — a graph of your own may read anything it puts there. What the **built-in** graphs read is a contract between two services, Chat and Engine, and a contract is a type: `common::execution_input::ExecutionInput` declares it once, Chat builds its `input_json` from it, and Engine's `llm` and `tool` executors read `state` back through it. A constant naming the key would not have been enough — each side would still spell the field itself, and a rename on one side would compile on both. It lives in `common/src/` because that is where a contract one service publishes for another belongs, and neither side can drift from it without failing to compile.

Today that is one field, `question`. A second field is added to the type, not to a `json!` literal at the call site.

The `executions` row, its step-0 checkpoint and the `NOTIFY` go in **one transaction**. That input lives only in the checkpoint's `state`, so a start that committed the row alone would leave a perfectly claimable `Ready` execution that ticks with empty state and answers a question nobody asked.

## The Event Contract

`StreamEvents` yields `engine.v1.ExecutionEvent`, and every field of it is declared:

- `payload_kind` is the `ExecutionEventKind` enum, not a Rust variant name. Renaming a variant in `engine_core::ExecutionPayload` changes nothing a client sees.
- `Execution.status` is the `ExecutionStatus` enum for the same reason. `wire::status_to_proto` maps the engine's own `Status` onto it in one exhaustive match, so a status the engine grows is a compile error until the wire declares it.
- `payload_json` carries the kind's **own fields** — the inner object — never an envelope keyed by the kind. A kind with no fields sends `{}`.
- `result` is the graph's resolved answer, set on `EXECUTION_COMPLETED` only.
- `error` is the failure text, set on `EXECUTION_FAILED` and `NODE_FAILED`.

`stream.rs` is the projection from storage onto that contract. `engine.execution_events.payload` holds engine-core's externally-tagged serde form — an object `{"NodeStarted": {…}}` for a variant with fields, a bare JSON string `"Resumed"` for one without — and `stream.rs` reads it by **deserializing it back into `engine_core::ExecutionPayload`** and matching the variants exhaustively. No variant spelling is written down a second time, so a rename in engine-core moves the whole projection with it or fails to compile, and `result` and `error` come off the variants that declare them rather than off field names read back out of JSON. A row this binary cannot deserialize — written by an engine whose payloads have since been renamed or restructured — becomes `EXECUTION_EVENT_KIND_UNSPECIFIED` and is logged: that is the same tolerance the old kind table had for an unknown kind, now with no table to keep in step.

`GetExecution` returns identity and `status` only. The run-time representation it used to ship as opaque strings — `state_json`, `current_nodes_json`, `wait_kind_json`, `iteration` — was unreadable without linking `engine-core` and had no consumer; those field numbers are reserved. A client that needs to know what an execution did reads its events.

## Tables

| Table | Class | Bound | Hot-path query |
|---|---|---|---|
| `executions` | Hot working set | Live executions plus terminal ones inside `ENGINE_TERMINAL_RETENTION` | `lease::claim_one_ready` per tick; `sweep::sweep_terminal` per fallback interval |
| `checkpoints` | Hot working set | At most `max_iterations` per live execution; swept with their execution | `store::PgCheckpointStore::latest` per tick |
| `graphs` | Reference data | One row per registered `(graph_id, version)` | `tick::load_execution_and_graph`, by the unique key |
| `schedules` | Reference data | The standing instructions users keep; a fired schedule updates in place | `scheduler::claim_one_due` per poll |
| `execution_events` | Append-only log | Monthly partitions; `ENGINE_EVENTS_RETENTION_MONTHS` (unset = forever) | `stream.rs`, in both subscription scopes |

**`checkpoints.state` is the source of truth for an execution's state**; there is deliberately no `state` column on `executions`. What `executions` does carry is `current_nodes`, a denormalized copy of the latest checkpoint's position written in the same transaction as that checkpoint, so `GetExecution` never has to join `checkpoints`. `schedules.graph_version` is nullable, and NULL means "whichever version is latest at fire time".

Each polling loop gets a partial index covering exactly the rows its query can pick, so neither loop pays for the other half of the table. That only works if the query's own `WHERE` implies the index predicate — Postgres will not use a partial index otherwise — so `CLAIMABLE_STATUSES` and the claim index must list the same statuses, `waiting` included, because an execution parked on an elapsed `WaitKind::Timer` is claimable. A test asserts the index predicates list exactly the statuses they are named for.

`StreamEvents` has two scopes and therefore two indexes. An execution-scoped subscription filters `execution_id`; a **user-scoped** one filters `user_id` and polls every 200 ms for as long as a session is open, across every partition — so `execution_events_user_id_id_idx` is not optional.

`execution_events.event_id` deliberately carries no unique constraint. It is a fresh `Uuid::new_v4()` at every insert site, no query filters on it and no insert takes an `ON CONFLICT` path against it, so a unique index would be a second b-tree maintained on every append of the highest-volume table for no read and no deduplication. Earning it back would mean deriving `event_id` deterministically at the producer, which `engine_core::Event` does not do.

## Sanctioned Raw SQL

Everything that reads or writes application rows goes through SeaORM entities and the query builder — leases included, `FOR UPDATE SKIP LOCKED` and the one jsonb predicate the builder has no method for (`Expr::cust_with_values` inside an otherwise typed `Select`). The exceptions are schema objects and catalog reads the ORM cannot express at all:

| Where | What | Why it cannot be an entity |
|---|---|---|
| [`entity/execution.rs`](src/entity/execution.rs), [`entity/schedule.rs`](src/entity/schedule.rs), [`execution_event.rs`](src/execution_event.rs) | `CREATE INDEX IF NOT EXISTS`, literal, run once after schema-sync | Schema-sync has no derive attribute for a **partial** index |
| [`execution_event.rs`](src/execution_event.rs) | The `PARTITION BY RANGE (occurred_at)` parent | Schema-sync emits a plain `CREATE TABLE`, and is not inert against a partitioned one it finds |
| [`partition.rs`](src/partition.rs) | `CREATE TABLE ... PARTITION OF` / `DROP TABLE` for the monthly children | Partition DDL has no query-builder form, and `FOR VALUES FROM (…) TO (…)` takes no bind parameters |
| [`partition.rs`](src/partition.rs) | `pg_get_partkeydef` and `pg_inherits` reads through `query_one_raw`/`query_all_raw` | Partition management has to ask the catalog what partitions exist; there is no entity for `pg_inherits`, and these read no application row |
| `NOTIFY engine_tick` | `execute_unprepared` at the end of each commit | `LISTEN`/`NOTIFY` is not a row operation |

Every value interpolated into a partition statement comes from `chrono` arithmetic or from a name this service itself formatted — never from a request.

Errors never carry any of this outward. `EngineError::Db` and `EngineError::Json` log their detail and return a fixed internal message, so no table, column, constraint or SQL fragment reaches a caller.

## Runtime

- **Tick loop** — `wakeup.rs` waits on `LISTEN engine_tick` with a fallback interval, then runs up to `LEASE_CONCURRENCY` ticks. Each tick claims one execution under `FOR UPDATE SKIP LOCKED`, runs its active nodes, calls `engine_core::step()`, and commits checkpoint, events and status in one transaction.
- **One engine at a time** — startup takes the `pg_try_advisory_xact_lock` in `lease::claim_sole_engine_lock` on a transaction it then holds open for the life of the process, and a second engine that cannot take it **refuses to start** instead of becoming a second ticker. Single-replica is therefore an enforced requirement rather than a convention, and Postgres releases the lock by itself when the holding process dies, so nothing has to be cleaned up after a crash.
- **Lease recovery** — `expire_abandoned_leases` runs once at startup, behind that lock, so an execution a replaced container left `running` is claimable immediately rather than after its five-minute lease. Holding the lock is what makes it safe: it is the proof that no other engine is alive, so every live lease it expires belongs to a process that is gone. Without that proof the same statement would yank a running replica's leases and set two workers on one execution's nodes. A genuinely multi-replica engine would need a heartbeat-renewed lease or a worker registry, and then no startup sweep at all.
- **Scheduler** — a plain interval poll, not `NOTIFY`: a cron run a few seconds late is invisible.
- **Sweep** — terminal executions and their checkpoints go after `ENGINE_TERMINAL_RETENTION`. `execution_events` is never swept; it is the durable history the sweep is safe because of, and `StreamEvents` is the client's contract for it. Checkpoints are deleted before their executions: the two go in one transaction, but the order records the dependency, since an execution row without its checkpoints is one the tick loop can no longer restart. `SWEEP_BATCH` bounds a single pass so the transaction stays short next to the claim query's `FOR UPDATE SKIP LOCKED`, and the next fallback tick takes the next batch.
- **Maintenance** — the sweep and the partition pass both hang off the fallback-interval arm and **never** off a `NOTIFY`, because a busy engine is exactly when `NOTIFY` fires constantly and must not pay for maintenance per commit. They share one permit between them: the interval fires far more often than a sweep can take, and two overlapping passes would contend for the same rows. Retention drops whole months only — a partition still holding part of the window is left alone, so retention is month-granular by construction.

## The Agent Loop's Tool Contract

`agent_graph` is `llm →[Truthy(/llm/tool_call)] tool → llm`, with `llm →[Not(Truthy(/llm/tool_call))] End`. The `tool → llm` edge is unconditional, so it forms the cycle, bounded by `max_iterations` and the budget rather than by an edge — `START → LLM ⇄ Tool → END`.

An `llm` node's output is always `engine_core::LlmOutput` — `{"tool_call": engine_core::ToolCall | null, "reply": string | null}`. llm-router's `CompleteResponse` is a plain `content: String` with no native tool calling, so the executor gets there by asking the model to answer in that JSON shape and parsing `content` as JSON. Every built-in graph writes that whole object to state under the key `"llm"`, which is why `llm_reply_pointer()` resolves whichever of them ran.

Those two types are declared once, in `engine-core`, and everything that needs the shape is built from them: the prompt paragraph asking for it is `serde_json` output of an `LlmOutput` holding placeholder text, the executor parses the model's answer straight into `LlmOutput`, the tool executor parses each requested call into `ToolCall`, and `llm_tool_call_pointer()` — the pointer `agent_graph`'s edges branch on and the tool executor reports in its error — is built from the same state key and field name. A field renamed in the type changes the prompt, the reader and the pointer together, or it does not compile.

**`config["tool_calling"] = true` is what wires the loop up.** It is the flag the Dispatcher reads before injecting the live catalog into that node's prompt; without it the `llm ⇄ tool` cycle is structurally present but the model never learns that any tool exists to call.

**A tool node learns what to call two different ways, and a graph picks one.** `agent_graph`'s tool node has empty `config` and an LLM decides the call, so it lands at `state["llm"]["tool_call"]`. `rag_graph`'s tool node *is* the entry point — no `llm` node has run yet, so there is no `state["llm"]` to read — and instead fixes its slug in `config["tool_slug"]`, searching the `ExecutionInput` question `StartExecution` began the execution with. A config-given `tool_slug` wins when present; otherwise the executor falls back to the `state["llm"]["tool_call"]` convention.

**A tool error is an observation, not a crash.** A tool that *ran* and said no — bad arguments, a site that blocks bots, a timeout — makes the tool node **succeed** with `{"error": "<message>"}`. The node's `Append` reducer lands that in `state["tool_result"]` like any other result, the unconditional `tool → llm` edge carries it straight back into the next prompt, and the model can retry with fixed arguments, choose another tool, or answer with what it has. The bound lives in the tool executor rather than in an edge, because only it can see the tail of `state["tool_result"]`: `MAX_CONSECUTIVE_TOOL_ERRORS` error observations in a row return `TaskError::Failed` instead, and — `agent_graph` having no `Condition::Failed` edge — that fails the execution with the last error in the message. A tool that could not be *asked* at all (Tool Service unreachable, an RPC transport error) or whose answer needs an approval Engine cannot give still fails on the first try.

The reducer is `Append` and not `Replace` on purpose: one turn can call more than one tool across loop iterations, and `Replace` would let the second result overwrite the first before the model ever saw both. `state["tool_result"]` is therefore an array, and `apply_reducer`'s rule for it is that a non-array existing value is replaced with a new single-element array rather than silently dropped, so `Append` on a fresh key behaves exactly like `Append` on one already holding a list.

**The prompt does not grow with the loop.** Only the last `RENDERED_TOOL_RESULTS` entries are rendered, each cut to `MAX_CHARS_PER_RENDERED_TOOL_RESULT`, and each keeps the index it has in the full log so the model can see that earlier calls happened even when they are no longer shown. Without that, a long-running execution walks into the token budget by accident. An `{"error": …}` entry is labelled as a failed call it may recover from, so it is not read as data the tool returned.

A response that is not the requested `{tool_call, reply}` JSON is treated as a plain final answer rather than a failure: a model that ignores the contract should still produce an unstructured but usable result. When the graph asks for the JSON contract, the provider is also asked for a JSON-object response format, which makes that the happy path rather than the fallback.

## Fan-Out and Fan-In

Three rules in `step()` that are not obvious from the node types:

- **The barrier's size is fixed at spawn time and never recomputed.** Re-reading `source` when the first branch happens to complete would see whatever state another output in the same super-step left behind — a `Replace` shrinking the very array that was fanned out over — sizing `results` too small for the branches actually spawned.
- **A failed branch is an ordinary failed `Task`.** Only its `Condition::Failed` edges may fire, and with none the whole execution hard-fails. Folding an `Err` into the barrier's `results` would be indistinguishable from "not reported yet" and would stall the barrier forever instead.
- **A satisfied `FanIn` is expanded in the same tick, not parked.** It never goes through a `TaskExecutor`, so it cannot wait for a future output the way a `Task` does; its outgoing edges are evaluated against state immediately and its targets pushed into `next`.

`Graph::validate` rejects a graph whose `FanOut` target does not lead to exactly one `FanIn`, so `step()` assumes that shape rather than re-checking it every tick — otherwise "which barrier does this branch's result belong to" has no single answer.

## What `step()` Cannot Do Alone

`engine-core` is pure and synchronous, so two node kinds need the service to meet them halfway:

- **`Subgraph`** parks as `Waiting(ExternalEvent { key: <its own node id> })` — the same event shape a `Wait` node emits, so a `StreamEvents` watcher sees "blocked on something" uniformly either way. `step()` cannot start a child execution, which needs Postgres, so it can only ever reach such a node with empty `outputs`. **This service does not wire the other half yet:** a tick that finds a `Subgraph` node in `current_nodes` fails the tick saying so, rather than pretending to run it.
- **`interrupt()`** is only the pure half of resuming a `Wait::UserInput` execution: it records the new input under a reserved state key and flips the status back to `Ready`. Writing that to Postgres and sending `NOTIFY` is the service's job.

`engine-core`'s `ExecutionEvent` is its **own** shape and free to change; what other services see is the declared `engine.v1.ExecutionEvent` contract that `stream.rs` projects the stored form onto, and `Event<P>` is deliberately not a shared `common::` type. A `Checkpoint`'s `schema_version` is a plain constant for the same reason a generic `Versioned<T>` would be premature: there is nothing to upgrade from yet.

`engine-core/src/replay.rs` is the proof rather than the assertion that "everything is in the log": replaying an execution's event log reconstructs the same status and state `step()` itself produced, which is the premise both crash recovery and Chat reading Engine's history rest on.

## What The Budget Charges

A completed `Task` costs exactly one tool call, whatever its kind, plus its token usage read off the output's own `{"usage": {"tokens_in", "tokens_out"}}` block. `LlmTaskExecutor` fills that block in from llm-router's `CompleteResponse`; a kind that reports no usage charges zero tokens. Without it a completed `Task` would only ever cost its one tool call, so the token limit would never bind on the one node kind that actually spends tokens. A turn asked again after a guard rejected its reply carries both turns' usage, and every typed decision it spent is charged beside them — the discarded work was really paid for.

**Wall time is not charged in `step()`.** `step()` is pure and synchronous and has no elapsed-time signal for the work that produced the outputs it is handed; charging it would need the runtime to time the `TaskExecutor` call and report it alongside the output, which no port does today. `Budget`'s fields are remaining capacity rather than configured limits, so `charge()` saturates at zero instead of underflowing and `exhausted()` is the single check `step()` needs before another iteration.

## One Claim, Three Reasons

`lease::claim_one_ready` is the only `FOR UPDATE SKIP LOCKED` in the service, and one query serves what would otherwise be three subsystems: a fresh `Ready` row is **new work**, a row whose lease expired is **crash recovery**, and a row parked on a `WaitKind::Timer` whose `until` has passed is **the clock as an external event**. There is no separate sweep for abandoned work and no timer daemon.

That third disjunct is the one predicate the query builder has no named method for: `wait_kind` is jsonb holding `WaitKind`'s externally tagged serde shape (`{"Timer": {"until": "<rfc3339>"}}`), so "this timer elapsed" is a jsonb path plus a `timestamptz` cast. The `?` in it is Postgres' jsonb key-existence operator, not a bind placeholder.

**The claim deliberately leaves `wait_kind` untouched.** The tick that picks the row up re-runs its `current_nodes` — still the `Wait` node — with a synthetic `Ok(Value::Null)` output, and `step()`'s ordinary per-output edge evaluation walks past it. That is the same unparking path `Resume` relies on, so waking on a timer is not a special code path; `release_lease` overwrites `status`/`wait_kind` at commit time.

**A commit names the lease it is releasing.** `release_lease` filters on `lease_owner` as well as `id` — the tick passes its own owner, and `Interrupt`/`Resume`/`Cancel`, which only ever touch an execution no worker holds, pass none and match `lease_owner IS NULL`. A worker whose lease expired mid-tick and was re-claimed therefore updates no row, and `commit_step` rolls its whole transaction back rather than writing a checkpoint for a step it no longer owns. Before, only the `checkpoints (execution_id, step)` unique key stood between a stale worker and overwriting the new owner's `status`, `current_nodes` and lease — an invariant enforced in a different module by accident.

`LEASE_DURATION` is five minutes because it stands in for a heartbeat that does not exist: nothing renews the lease mid-tick, so a lease shorter than the worst realistic tick (one `TaskExecutor::execute` — up to `RETRY_ATTEMPTS` calls of `CALL_TIMEOUT` plus backoff) would let a second worker reclaim and re-run it. A heartbeat task extending the lease mid-tick would let this shrink back.

## Who May Read An Execution

Identity comes from the `Principal` headers Gateway stamps, **never** from a body field — the same rule `StartExecution` already follows:

| Call | Rule |
|---|---|
| `StreamEvents`, user-scoped | The body's `user_id` is whatever the caller typed, so it is ignored entirely and the filter comes from the Principal. No Principal means there is no user to scope to, and the call is rejected rather than falling back to the body |
| `StreamEvents`, execution-scoped | An execution that *has* a `user_id` is streamable only by that user. One with none stays open, so the unauthenticated path still works |
| `ListSchedules` | The owner comes from request metadata, never a body field; a caller with no Principal sees the system schedules (`user_id IS NULL`), not everybody's |

`Resume` takes a narrower status guard than `Interrupt` and `Cancel`, which legitimately accept a broader set: `Resume` means only "the thing this execution was waiting for happened", and without the guard it would happily revive a `Completed`/`Failed`/`Cancelled` execution back to `Ready`. It used to take a `wait_key` alongside that guard and drop it unread, which is a worse contract than not offering the field: a caller could name any key and be resumed anyway. `ResumeRequest` field 2 is `reserved` instead, because every `Wait` a graph can reach today is either `UserInput`, which `Interrupt` covers, or a `Subgraph`'s `ExternalEvent`, which is not wired at the service level — so there is nothing yet to check a key against. It comes back with a new number, and a real check against the execution's `WaitKind`, when approvals and `Subgraph` need it.

## Cron Expressions

`schedules.cron_expr` uses the `cron` crate's field convention, **not** classic 5-field crontab: six fields (`sec min hour day-of-month month day-of-week`) with an optional seventh for the year. `"0 0 * * * *"` is therefore hourly on the hour, and a bare five-field `"* * * * *"` is **rejected** rather than silently reinterpreted.

`CreateSchedule` validates an expression by computing its first fire time, which doubles as the row's initial `next_run_at` — an expression that can produce none (malformed, or pinned to a year already past) is refused at the entry instead of becoming a row the scheduler can only log about. Standing exactly on a fire time must advance rather than return the same instant, or the scheduler would re-claim the row forever.

A tick missed while the service was down is **not** replayed: the next fire time is computed from now, exactly like `crond`. The claim and the advance of `next_run_at` happen in one transaction, which gives at-most-once-per-due-tick semantics across any number of instances — whoever wins the row also moves it forward, so nobody else sees it as due. A row whose `cron_expr` no longer parses (hand-edited, or written before that validation existed) is **disabled** rather than fired; leaving `next_run_at` in the past would make every poll re-claim the same unfireable row forever. Starting the execution happens after the claim transaction has committed and is best effort: the schedule is advanced either way, so a graph that no longer exists costs one skipped run and a log line, never the loop.

## Schema-Sync Boundaries

`get_schema_registry("engine::entity::*")` selects registry entries by module path, so **where an entity lives is a decision, not a filing convention**. Anything under `crate::entity::` is created and kept in step by schema-sync with no further wiring; `crate::execution_event` is deliberately outside it because its table is a partitioned parent schema-sync can neither create nor leave alone.

"Leave alone" is the part worth spelling out, measured against sea-orm 2.0.2: with `#[sea_orm(unique_key = …)]` on `event_id`, every sync fails outright with *"unique constraint on partitioned table must include all partitioning columns"*; without it, sync **silently drops** the table's own `UNIQUE (occurred_at, event_id)` — its last pass removes unique keys it finds in the database but not on the entity. So the table is created by literal statements before any partition management, and schema-sync never sees it.

Startup asks the catalog whether `execution_events` really is the partitioned parent, because there is exactly one way `CREATE TABLE IF NOT EXISTS` can quietly do nothing: a database still holding the plain table from before partitioning. The service keeps working in that state — same columns — but silently without the retention path, so startup says so out loud rather than leaving it to be discovered later.

Startup is also tolerant of a misspelt retention: `ENGINE_TERMINAL_RETENTION` that is unset or unreadable falls back to the default rather than refusing to boot. `ENGINE_EVENTS_RETENTION_MONTHS` unset keeps every partition forever — the event log is the durable history everything else here is allowed to be swept against, so dropping from it is an explicit decision and never one made by omission.

## Config

`DATABASE_URL`, `LLM_ROUTER_URL`, `TOOL_SERVICE_URL`, and the two optional retentions above.
