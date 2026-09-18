# System One, Used Properly — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the typed-decision path actually fast, and make our local contract agree with the vendor's, so a request our validator accepts is never refused upstream after a paid hop.

**Architecture:** Five independent changes, ordered so that each one's risk is lower than the next. The first four are corrections — a deadline that makes "advisory" true, limits that match the vendor's, a budget read once instead of discovered by refusal, and a validation that stops paying for prose. The fifth changes the agent graph's control flow and lands only if everything before it is green.

**Tech Stack:** Rust 2024, Connect RPC (`connectrpc` + `buffa`), TypeSafe AI System One via `services/llm-router`, SeaORM, `cargo-nextest`.

**Source:** a research pass over `https://docs.typesafe.ai` against this workspace. Its findings are restated inline per task; where our repository contradicted the vendor, the task says so.

## Global Constraints

- Cargo is not on the default PATH: prefix every shell command with `export PATH="$HOME/.cargo/bin:$PATH"`.
- Strict lints on every file: `unsafe_code = "forbid"`, clippy `all = deny`, `unwrap_used = "deny"`, `too_many_lines = "deny"`, `cognitive_complexity = "deny"` (threshold 20), `too_many_arguments = "deny"`.
- No `async_trait`; ports return `impl Future<Output = ...> + Send` in return position.
- Entity-first SeaORM. No hand-written `.sql`.
- Provider credentials exist only in `services/llm-router`. No other service holds a key or names a model.
- `.agents/skills/code-review/gate-architecture/SKILL.md`'s "Capabilities, Not Topics" binds every task: no domain, topic or vendor subject in code, prompts, or test fixtures.
- **A decision is advisory.** One vendor serves this class, so there is no tier and no fallback. Every change must say what happens when `Decide` is unavailable, and a failure must never lose a user's turn.
- Docker must be running for database tests; `protoc` on PATH for proto changes.

---

### Task 1: A deadline that makes "advisory" true

**Why.** `services/chat/src/intent.rs` gives `Decide` a 30-second `CALL_TIMEOUT`, and `services/llm-router`'s client to the vendor allows 60. The vendor answers most queries in about 100 ms, and one published measurement put a 13-question call at 0.27 s. So on a bad day a user waits half a minute to receive the deterministic fallback that was supposed to make failure invisible. The fallback exists; the deadline makes it a lie.

**Files:**
- Modify: `services/chat/src/intent.rs` (`CALL_TIMEOUT`)
- Modify: `services/llm-router/src/adapters/system_one/mod.rs` (its own HTTP timeout, today shared with the completion side)
- Test: inline tests in both

**Steps:**
- [ ] Give `Decide` its own deadline constant, separate from `Complete`'s, at **2 seconds** end to end in Chat, with a comment stating the vendor's published ~100 ms and why the multiple is 20× rather than 2× (a cold connection, a retry inside the vendor, and our own hop).
- [ ] Give the System One adapter its own request timeout of the same order, not the 60 s the completion adapter needs. Completion timeouts must not change.
- [ ] Write a test that a decider which never answers falls back **within** the deadline, not after the old one. Use `tokio::time` pausing rather than really sleeping.
- [ ] Do **not** add a retry. The vendor's SDK retries twice by default, but this service's stated rule is that a failure is returned rather than retried, and a retry inside a 2 s deadline buys little. Record that as a deliberate choice in the adapter's comment.
- [ ] Commit: `chat: a decision gets two seconds, so the fallback is reached while it still helps`

---

### Task 2: Limits that match the vendor's

**Why.** Three places where our contract and the vendor's disagree. The first is a real defect: our `MAX_STATE_BYTES` is 256 KiB, roughly 65k tokens, while the vendor's ceiling is 32k tokens for state plus the longest question. A request our validator accepts can be refused upstream — after we have paid for the hop. The other two are documentation that claims vendor authority for our own choices.

**Files:**
- Modify: `services/llm-router/src/budget.rs` (`MAX_STATE_BYTES`, and the comment on `MAX_QUESTIONS`)
- Modify: `services/llm-router/README.md`
- Modify: `common/proto/llm_router/v1/llm_router.proto` (the comments on `Question`, `Choice` and `DecisionBudget`)
- Test: inline tests in `budget.rs`

**Steps:**
- [ ] Lower `MAX_STATE_BYTES` to a value that cannot exceed the vendor's 32k-token state ceiling. Bytes are a proxy for tokens, so pick the conservative side and say so in the comment: at roughly 4 bytes per token, 128 KiB is the honest cap. Update the existing budget tests to the new number.
- [ ] `MAX_QUESTIONS = 64` stays, but its comment and the README must stop implying the vendor sets it. The vendor caps tokens, not count. Say that 64 is ours, and why: a request that needs more questions than that is asking about too much at once.
- [ ] **Correct the option-order rationale.** The README and the proto say `repeated` is used "because the order reaches the model". The vendor's HTTP API takes questions and choice criteria as **maps keyed by id**, and evaluates each question in isolation; only `score.levels` is documented as ordered. Keep the wire shape — `repeated` is still right, because it is stable and a proto map has no defined order — but replace the reason with the true one, and keep the ordering claim only where it holds.
- [ ] Add a test that a state one byte over the new cap is refused and one exactly at it is accepted.
- [ ] Commit: `llm-router: our limits stop claiming the vendor's authority, and state fits its ceiling`

---

### Task 3: Read the budget once instead of learning it by refusal

**Why.** Chat's `route` question carries one option per live topic plus `new`. A long session can exceed the option ceiling, and today that turn is refused per call and silently falls back — paying a hop each time to learn a constant.

**Files:**
- Modify: `services/chat/src/intent.rs`
- Modify: `services/chat/src/main.rs` (fetch once at startup)
- Test: inline tests in `intent.rs`

**Steps:**
- [ ] At startup, call `DescribeModels` once and keep its `DecisionBudget`. On any failure, use the proto defaults and log at `warn` — **never fail startup**. Decisions are advisory, so the budget is too.
- [ ] Cap `route`'s options at the budget's `max_choice_options`, sending the most recent topics plus `new`. A topic dropped from the options can still be chosen by the fallback, so nothing becomes unreachable.
- [ ] Test: a session with more topics than the cap sends exactly the cap, includes `new`, and prefers the most recent.
- [ ] Test: an unavailable `DescribeModels` at startup leaves Chat working on the defaults.
- [ ] Commit: `chat: the decision budget is read once, not discovered by being refused`

---

### Task 4: A refusal explains itself without a second model

**Why.** `validate_tool` asks one `noul` and then spends a **medium-tier** `Complete` to write the refusal. The vendor's own guidance is to decompose: ask one `noul` per criterion in the same call — extra questions are nearly free and barely move the latency — and compose the refusal in code from whichever criteria failed. That removes a whole generative call from the refusal path.

**Files:**
- Modify: `services/tool/src/service.rs` (`validate_tool`)
- Test: inline tests in the same file

**Steps:**
- [ ] Replace the single `meets_standard` question with one `noul` per criterion, all in one `Decide`: input schema plausible, output schema plausible, description states what the tool does, a write or destructive tool states what it changes, timeout plausible for the work. Keep an overall gate.
- [ ] Compose the refusal text in Rust from the criteria that failed. Delete the `Complete` call and its prompt.
- [ ] **Fix the instruction wording while here.** "against 2026 API design standards" is exactly the literal-reading trap the vendor warns about — a date in an instruction invites the model to reason about the date. Say what is required, not when.
- [ ] Test: a refusal names the criterion that failed and makes **zero** `Complete` calls — assert the counter, which already exists.
- [ ] Test: an unreachable decider still refuses rather than approving.
- [ ] Commit: `tool: a refusal is assembled from the criteria that failed, not written by a second model`

---

### Task 5: The agent decides before it generates

**Why.** This is the largest latency win and the largest risk. Today every agent turn spends a medium-tier `Complete` to decide *whether and which* tool to call, and a second one when the model answers without calling anything (the nudge). A typed decision can answer both questions in ~100-300 ms.

**Do not start this task unless Tasks 1-4 are green and the live exercise in Task 6 has been run against them.**

**Files:**
- Modify: `services/engine/src/executors/llm.rs`
- Modify: `engine-core/src/builder.rs` if the graph needs a node
- Test: inline tests in `llm.rs`

**Steps:**
- [ ] Before the first generative call, send one `Decide` whose state is the question alone — not the whole execution state; the vendor's guidance is to send only what the question needs. Two questions:
  - `needs_external_information` (`noul`): answering requires information not present in the state — something current, specific, or about this system's own data. Behavioural, naming no subject.
  - `tool` (`choice`): one option per entry in the **live catalog** the dispatcher already injects, plus `none`. The code names no tool; it reads the catalog, which is what the gate requires.
- [ ] When `needs_external_information` is confidently false: run the answering `Complete` with the nudge **disabled**. Today the nudge fires on structure alone and costs a second generative call on every greeting and every question needing no tool.
- [ ] When `tool` is confident **and** that tool's `input_schema` has exactly one required string field: call it with the question verbatim, append the result, and run `Complete` once. A tool needing structured arguments never takes this path — decided by its schema, never by its name. A wrong pick costs one tool call, not the turn, because the normal loop continues afterwards.
- [ ] Everything else: today's flow, unchanged.
- [ ] Fail-safe: any `Decide` error or timeout falls through to today's flow with the nudge enabled. Test it.
- [ ] Test: a confident "no external information needed" makes exactly one `Complete` call and no nudge.
- [ ] Test: a confident single-string-field tool is dispatched without a generative turn first.
- [ ] Test: a tool whose schema needs two fields is **not** taken on the fast path even when confidently chosen.
- [ ] Commit: `engine: a typed decision picks the tool, so the common turn spends one generative call`

---

### Task 6: Exercise it for real, then measure

**Why.** Every number above is the vendor's or a projection. This deployment already records both: `llm_router.decisions.latency_ms` and `llm_router.requests.latency_ms`.

**Steps:**
- [ ] Rebuild and restart the stack so the containers run this branch, not the images from before it.
- [ ] Drive real traffic through Gateway's public API: a turn that continues a topic, a turn that opens one, a turn carrying nothing to act on (the clarification path), and a turn whose answer needs a tool.
- [ ] Query both latency tables and report the real p50 and p99 for `Decide` against low-tier `Complete` in this stack. That comparison is the one number this whole plan rests on, and until now nobody has had it.
- [ ] Record the results in the plan's workspace and in `services/llm-router/README.md` if they are stable enough to publish.

---

### Task 7: A follow-up must not fail because the agent was mid-tick

**Why.** Found by the live check, not by any test. Sending "and the population?"
straight after a question returns `{"code":"internal","message":"chat could not
complete the request"}`, and `docker compose logs chat` gives the cause:

```
engine call failed: invalid_argument: execution ... is mid-tick, retry shortly
```

`services/engine/src/control.rs` refuses a message aimed at an execution that is
currently ticking. That refusal predates this whole branch — `git log -S
"mid-tick"` puts it in the original engine commit — and it was survivable only
because routing used to take ~1.4 s, which was long enough for the tick to
finish before delivery was attempted. Routing is now 150-300 ms, so a user who
types a follow-up immediately lands inside the tick and gets an error. **Speed
did not create this bug; it made it reachable.**

**Files:**
- Modify: `services/chat/src/topic_turn.rs` (the delivery path)
- Test: inline tests in the same file

**Steps:**
- [ ] Read `services/engine/src/control.rs` first and see exactly which refusal
      carries "mid-tick", so the retry matches that one case and nothing else. A
      retry on a genuine `invalid_argument` — a malformed request — would spin
      on an error that will never clear.
- [ ] Retry that one refusal, briefly and boundedly: a handful of attempts over
      a few hundred milliseconds, then give up and return what it returns today.
      The contract's own words are "retry shortly", so this is the behaviour it
      already invites; the caller simply never implemented its half.
- [ ] Keep the whole retry well inside the turn's own budget. A user waiting
      300 ms is fine; a user waiting two seconds for a follow-up is the latency
      problem this plan exists to remove, reintroduced by the back door.
- [ ] Test: a delivery refused as mid-tick once, then accepted, delivers and
      returns `Ok` — and does so without the caller seeing an error.
- [ ] Test: a delivery refused as mid-tick every time gives up and surfaces the
      error rather than retrying forever.
- [ ] Test: a refusal that is *not* mid-tick is returned immediately, with no
      retry at all.
- [ ] **Live check before committing**, per
      `.superpowers/sdd/2026-09-17-system-one-latency/smoke-check.md`. The proof
      this task works is the exact sequence that failed: ask a question, then
      immediately send a follow-up fragment, and get a topic id rather than an
      internal error.
- [ ] Commit: `chat: a follow-up waits out a tick instead of failing the turn`

---

## Explicitly not doing

- **Knowledge Base `page_type` as a decision.** Correct in principle and nearly free, but ingest is not on the turn path and the benefit is quality, not latency. Do it when `page_type` becomes a search filter.
- **Re-ranking `kb_search` hits.** The vendor's cookbook shows a real accuracy gain, but committing needs a hit-rate baseline this stack does not have. Measure first.
- **A decision between disagreeing declarative sources or fetched pages.** The `Complete` that reads them has to happen anyway, so a decision in between adds latency and removes nothing.
- **Running the theme-splitting `Complete` speculatively.** It is generation, and it would spend a generative call on every turn to save one on the few per cent that split.
- **`score`.** Every gate in this stack is binary or categorical. The primitive stays in the proto with no caller until a threshold over ordered levels actually appears.
