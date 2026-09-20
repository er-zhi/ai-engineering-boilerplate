# Capability Tree — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make what the application can do a declaration the engine routes with typed decisions, instead of something a generative model improvises each turn — but only where that arithmetic actually pays, and prove it pays before building the machinery.

**Architecture:** Three tasks land first and are worth their own keep: the transcript learns to show when each line arrived and how long the gap before it was, the decision model gets pinned so tuned thresholds cannot drift under us, and a real declarative row goes in against real endpoints. That third task is a gate, not a formality — it decides whether any capability qualifies for the tree at all. Tasks 4 through 7 build the tree itself and are not started until the gate returns a ruling.

**Tech Stack:** Rust 2024, Connect RPC, SeaORM 2.0, plain static HTML for the client, the decision provider behind `services/llm-router`, the `race` primitive in `services/tool`.

**Spec:** [`docs/superpowers/specs/2026-09-20-capability-tree-design.md`](../specs/2026-09-20-capability-tree-design.md) — read it first; the rulings R1–R8 are what this plan argues from, and R1 is the one that decides how much of it gets built.

## Global Constraints

- Cargo is not on the default PATH: prefix every shell command with `export PATH="$HOME/.cargo/bin:$PATH"`.
- Strict lints: `unsafe_code = "forbid"`, clippy `all = deny`, `unwrap_used = "deny"`, `too_many_lines = "deny"`, `cognitive_complexity = "deny"` (threshold 20), `too_many_arguments = "deny"`.
- Provider credentials and model names exist only in `services/llm-router`. No vendor or provider name may appear in Rust anywhere.
- `.agents/skills/code-review/gate-architecture/SKILL.md`, "Capabilities, Not Topics": no domain, topic or vendor subject in code, prompts or fixtures. Weather and market endpoints live only in the operator's file, which is gitignored.
- **Verify the live chat before every commit** — `.superpowers/sdd/2026-09-17-system-one-latency/smoke-check.md`. A green test suite is not evidence the product works.
- No regular expressions for argument extraction (spec R2), and no per-capability conditional in Rust (spec R7).
- `services/frontend` has no bundler and no test framework: its container copies `client/` into a volume. A frontend change is verified in the browser and through the smoke check, not by a unit test.

---

### Task 1: Every transcript line carries its time and the gap before it

**Why.** Every latency claim in the last two rounds of this work came from SQL against the audit tables. The person using the product could not see any of it. A turn that takes 2 s and a turn that takes 9 s look identical in the transcript, so a regression is invisible until someone runs a query. This is also the instrument Task 3 is measured with.

**Files:**
- Modify: `services/frontend/client/chat.html` (CSS block at line 22, `appendMessage` at line 263, `renderTranscript` at line 361, `startOver` at line 464, and the seven other `appendMessage` call sites)
- Modify: `services/frontend/README.md` (the Pages table is missing `/chat` entirely)

**Interfaces:**
- Produces: `appendMessage(who, text, className, topicTitle, at)` — `at` is an RFC 3339 string or `undefined`, which means "now". Every later task that appends a line passes the time the thing actually happened, never the time it was rendered.

- [ ] **Step 1: Add the stamp's styling**

In the CSS block, after the `.msg .kind` rule on line 24, add:

```css
  .msg .at { color: #aaa; font-size: .75em; font-variant-numeric: tabular-nums; }
  .msg .at .gap { color: #777; }
```

Tabular numerals matter: without them the stamps jitter column by column and the eye cannot scan the gaps down the page, which is the entire point of the change.

- [ ] **Step 2: Add the two formatters and the cursor they share**

Immediately above `function appendMessage` (line 263), add:

```js
  // The time of the previously rendered line, so each line can show the gap before it. Reset
  // wherever the transcript is cleared: a stale cursor would print a gap measured against a line
  // that is no longer on screen.
  let previousAtMs = null;

  function clockText(iso) {
    const at = new Date(iso);
    if (Number.isNaN(at.getTime())) return "";
    const ms = String(at.getMilliseconds()).padStart(3, "0");
    return `${at.toLocaleTimeString([], { hour12: false })}.${ms}`;
  }

  // Only ever a forward gap. Entries are sorted before rendering, but a live event can still
  // arrive stamped behind the last rendered line, and "-0.4s" would read as a bug rather than as
  // the clock skew it is. Two clocks reach here: server times for anything the server recorded,
  // and the browser's own for the three lines that only ever happen locally — the optimistic echo
  // of a message being sent and the two error lines. A browser clock running ahead therefore
  // suppresses the gap on the next server-stamped line rather than printing a wrong one, which is
  // the direction this should fail in.
  function gapText(stampMs) {
    if (previousAtMs === null || !Number.isFinite(stampMs)) return "";
    const seconds = (stampMs - previousAtMs) / 1000;
    return seconds > 0 ? ` +${seconds.toFixed(1)}s` : "";
  }
```

`atMs` is already defined at line 295 and hoists, so it is callable from here.

- [ ] **Step 3: Render the stamp**

Replace `appendMessage` (line 263) with:

```js
  function appendMessage(who, text, className, topicTitle, at) {
    const div = element("div", `msg ${className || ""}`);
    const stamp = at || new Date().toISOString();
    const stampMs = atMs(stamp);
    const label = clockText(stamp);
    if (label) {
      const span = element("span", "at", label);
      const gap = gapText(stampMs);
      if (gap) span.appendChild(element("span", "gap", gap));
      span.appendChild(document.createTextNode(" "));
      div.appendChild(span);
      if (Number.isFinite(stampMs)) previousAtMs = stampMs;
    }
    if (topicTitle) div.appendChild(element("span", "topic", `[${topicTitle}]`));
    div.appendChild(element("span", "kind", `${who} `));
    if (who === "assistant") {
      const body = element("span");
      body.innerHTML = renderMarkdown(text);
      div.appendChild(body);
    } else {
      div.appendChild(document.createTextNode(text));
    }
    transcript.appendChild(div);
    transcript.scrollTop = transcript.scrollHeight;
    return div;
  }
```

- [ ] **Step 4: Reset the cursor wherever the transcript is cleared, and pass the real times**

In `renderTranscript` (line 361), after `transcript.textContent = "";` add `previousAtMs = null;`, and pass the entry's own time:

```js
      appendMessage(entry.who, entry.text, entry.className, entry.topic, entry.at);
```

In `startOver` (line 464), after `transcript.textContent = "";` add `previousAtMs = null;`.

In `renderEvent`, the four live appends take the event's own time rather than the render time — the gap is only meaningful if it measures the server, not the browser:

```js
        appendMessage("you", payload.content || "", undefined, undefined, event.occurredAt);
        appendMessage("assistant", payload.text || "", undefined, undefined, event.occurredAt);
```
```js
        appendMessage("·", payload.text || payload.kind || "", "notification", rootTitleOf(topics, event.topicId), event.occurredAt);
```
```js
        appendMessage("·", progressText(event.payloadJson), "notification", rootTitleOf(topics, event.topicId), event.occurredAt);
```

Leave the three remaining call sites (the optimistic echo at line 455 and the two error lines) without an `at`: they happen in the browser, and "now" is the honest time for them.

- [ ] **Step 5: Add `/chat` to the README's Pages table**

`services/frontend/README.md` lists `/`, `/sources` and `/login` but not `/chat`, which is the page this task changes. Add the row:

```markdown
| `/chat` | Send turns, watch topics run, and read the transcript with per-line timing. |
```

- [ ] **Step 6: Verify in the browser**

```bash
docker compose up -d --build frontend gateway
```

Log in at `http://localhost:8080/login` with `GATEWAY_AUTH_PASSWORD` from `.env`, open `/chat`, and send `what is the capital of Kyrgyzstan and what is the weather like there?`.

Expected: every line is prefixed with `HH:MM:SS.mmm`, the first line shows no gap, and each later line shows `+N.Ns` in red. The gap on the answer line is the number the last two rounds of work were spending SQL to find. Reload the page and confirm the rebuilt transcript shows the same stamps — they come from stored `createdAt`/`updatedAt`, so they must survive a reload unchanged rather than collapsing to the reload time.

- [ ] **Step 7: Run the check suite and the smoke check, then commit**

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo nextest run --workspace
```

Then the live check in `.superpowers/sdd/2026-09-17-system-one-latency/smoke-check.md`.

```bash
git add services/frontend/client/chat.html services/frontend/README.md
git commit -m "frontend: each transcript line shows when it arrived and how long the gap before it was"
```

---

### Task 2: Pin the decision model, so a tuned threshold cannot drift under us

**Why.** We call the alias, and the vendor's `models.md` says plainly: "If you have tuned confidence thresholds against a specific version, pin that version's ID instead of the alias." Tasks 4 onward tune thresholds — 0.35 for actionable, 0.5 for route, 0.75 for separate themes already exist in `services/chat/src/intent.rs` — and an alias that moves would shift all of them silently, with no failing test and no error in any log. This costs one line and closes that whole class of incident.

**Files:**
- Modify: `.env` (operator value; not committed)
- Modify: `.env.example` (the comment explaining the choice)
- Modify: `services/llm-router/README.md` (the `TYPESAFE_AI_MODEL` paragraph)

**Interfaces:**
- Consumes: nothing. Produces: nothing in code — the model name never appears in Rust, only in the operator's environment.

- [ ] **Step 1: Read what the deployment is actually answering with**

```bash
export PATH="$HOME/.cargo/bin:$PATH"
docker compose exec -T postgres psql -U postgres -d app -c \
  "SELECT model_used, count(*), max(created_at) FROM llm_router.decisions GROUP BY 1 ORDER BY 2 DESC;"
```

The alias resolves to a concrete version, and that version string is what gets pinned. **Pin only a version this query has actually shown.** Nothing validates the model name: `type_safe_ai` in `services/llm-router/src/main.rs:165-187` reads `TYPESAFE_AI_MODEL` and passes it straight through, and the one startup probe — `verify_key` at `services/llm-router/src/adapters/system_one/mod.rs:66` — is a `GET /v1/models` whose status is checked for the *key* alone; the configured model is never compared against what comes back. Checking against that list would not help either: the vendor states on `models.md` that versioned ids are accepted by the `model` field whether or not they appear in it. A typo therefore reaches a healthy container and fails on the first decision of the first turn.

- [ ] **Step 2: Pin it**

In `.env`, set `TYPESAFE_AI_MODEL` to the concrete version the query returned, replacing the alias.

- [ ] **Step 3: Explain the choice where the next operator will read it**

In `.env.example`, replace the two-line comment above `TYPESAFE_AI_MODEL` with:

```
# Both optional. The flagship alias tracks the newest official build, and a preview alias tracks the
# newest build of any kind. Pin a concrete version instead of an alias once you have tuned confidence
# thresholds against one: the vendor's own guidance, and an alias that moves would shift every tuned
# threshold at once with nothing failing to tell you.
```

In `services/llm-router/README.md`, add the same reasoning in one sentence to the paragraph that documents `TYPESAFE_AI_MODEL`'s default.

- [ ] **Step 4: Restart, confirm the pin took, then commit**

```bash
docker compose up -d llm-router
docker compose logs --tail 20 llm-router
```

A healthy container proves nothing here — see Step 1: the model name is never validated at startup, so a typo starts clean. **The only confirmation is a turn that actually decides.** Send one through the chat, then re-run Step 1's query: `model_used` must be the pinned version, and the decision count must have gone up. If the turn instead falls back to its generative path, the pin is wrong — check `docker compose logs llm-router` for the provider's rejection rather than trusting the health check.

```bash
git add .env.example services/llm-router/README.md
git commit -m "llm-router: pin the decision model, because a moving alias silently reprices every tuned threshold"
```

---

### Task 3: One real capability, and the measurement that decides whether the tree gets built

**Why.** This is the gate. Spec R1 says the tree pays only for capabilities whose arguments are all closed sets, and today **no declarative row exists at all** — the set of capabilities that would benefit is empty, so the tree would be machinery serving nothing. Before building it we establish, against real endpoints, (a) whether a capability with two closed arguments exists and behaves, and (b) whether a capability with an open-ended argument can be served at all by the existing single-string fast path, which passes the user's whole message as the argument. Answer (b) is one request to find out and it decides whether open-argument capabilities have any typed path.

The row is an ops artifact. It is written in the repo root, where `.gitignore` already covers it, and it is never committed. This task commits no subject — only the ruling, in the ledger.

**Files:**
- Create: `declarative-tools.json` in the repo root — **gitignored, never committed**
- Create: `.superpowers/sdd/2026-09-20-capability-tree/progress.md` (the ledger this plan's rulings are written into)

**Interfaces:**
- Produces: a `Ruling:` line in the ledger stating whether any capability qualifies under R1. Tasks 4–7 consume it and are not started without it.

- [ ] **Step 1: Find out whether a free-text source tolerates a whole question**

Pick an open geocoding endpoint that takes a free-text place query. Send it the user's whole message, exactly as the fast path at `services/engine/src/executors/llm.rs:646` would:

```bash
curl -sG 'https://<geocoder>/search' --data-urlencode 'q=what is the weather like in Bishkek' | head -c 400
echo
curl -sG 'https://<geocoder>/search' --data-urlencode 'q=Bishkek' | head -c 400
```

**Record the shape of the outcome, never the endpoint or the reply bodies.** The ledger is committed, and a later agent reads it as precedent when writing a fixture — a named third-party endpoint and a subject-bearing query sitting in it is exactly the leak the architecture gate exists to stop. One line is enough: *a whole-question query did / did not resolve to the same result as the bare term.* If it did, open-argument capabilities have a typed path after all and spec R1's third row changes. If it did not — the expected outcome — that is the evidence that such capabilities stay generative, written down so nobody re-litigates it.

- [ ] **Step 2: Write the row**

Write `declarative-tools.json` in the repo root with one capability whose arguments are **two closed sets** — this is the shape spec R1 says pays, and the task exists to test that shape, not a single-string one the fast path already handles. Use endpoints that need no key. The row is shaped exactly as `services/tool/README.md`'s "Declarative Tools" section documents: `{slug, name, description, timeout_seconds, input_schema, sources}`, with `fan_out`, `take` and a list of `{name, url, pick}`. Give it at least three sources so `take: 2` has a reserve to refill from, and describe it as a capability with no subject in the description's grammar, the way the seeded tools are described.

- [ ] **Step 3: Load it and confirm the whole file is accepted**

```bash
docker compose -f compose.yaml -f - up -d tool <<'EOF'
services:
  tool:
    environment:
      DECLARATIVE_TOOLS_PATH: /etc/tool/declarative-tools.json
    volumes:
      - ./declarative-tools.json:/etc/tool/declarative-tools.json:ro
EOF
docker compose logs --tail 30 tool
```

Expected: the log names the row as loaded. One bad row refuses the whole file, so a silent start with no row named is a failure, not a pass — check the log rather than assuming.

- [ ] **Step 4: Prove the row answers, and time it**

```bash
docker compose exec -T postgres psql -U postgres -d app -c \
  "SELECT slug, name FROM tool.tools WHERE user_id IS NULL ORDER BY slug;"
```

Then call the capability through the chat with a question that needs both of its arguments, and read the timing straight off the transcript — Task 1 put it there. Record in the ledger: the gap between the user's line and the answer, and whether the tool was dispatched without a generative argument call (check `engine.execution_events` for the node outputs, where a fast dispatch is recorded under its own field).

- [ ] **Step 5: Write the ruling**

Create `.superpowers/sdd/2026-09-20-capability-tree/progress.md` and record, as a `Ruling:` line:

- whether a capability with two closed arguments exists and answers correctly;
- what the turn cost, in generative calls and in seconds;
- whether a whole-question query resolved to the same result as the bare term (the shape only — no endpoint, no reply body, per Step 1);
- and therefore whether Tasks 4–7 are built, built in reduced form, or abandoned.

**Abandoning is a legitimate outcome and must be written as plainly as the alternative.** The review that produced spec R1 found the first draft of this design did not pay; if the measurement agrees, the honest result is a ledger entry saying so and a plan that stops here.

- [ ] **Step 6: Commit the ledger only**

```bash
git status --short   # confirm declarative-tools.json does NOT appear
git add .superpowers/sdd/2026-09-20-capability-tree/progress.md
git commit -m "docs: what one real declarative capability costs, and whether the tree is worth building"
```

---

## Gated on Task 3's ruling

The four tasks below build the tree. **They are not started until Task 3 records a ruling that the gain set is non-empty.** They are specified here so that the ruling has something concrete to approve or reject, not so that they are executed by default.

### Task 4: A `decide` node kind, so the tree's branches are data

**Files:** create `services/engine/src/executors/decide.rs`; modify `services/engine/src/dispatch.rs` (`NodeKind`), `services/engine/src/executors/mod.rs` and `services/engine/src/main.rs` (hand the **existing** decider to the new executor), `services/engine/README.md`.

**Engine already holds a decider — do not construct a second one.** `LlmTaskExecutor` owns a `SystemOneServiceClient` at `services/engine/src/executors/llm.rs:294`, wired at `:311` against `LLM_ROUTER_URL`, and its fast path calls `Decide` today. Build one client in `main.rs` and pass it to both executors. There is exactly one `DECIDE_CALL_TIMEOUT`, and it stays where it is documented (`llm.rs:120-132`): 2 s, which must remain strictly above llm-router's own 1.5 s request timeout so the inner deadline fires first and the audit row survives. A second declaration of that constant is a second place to get that ordering wrong.

The node's config carries the questions and **their thresholds**. It writes into state the chosen label and one boolean per thresholded question — never a raw float. Spec R5: `engine_core::Condition` has no numeric comparison, and extending it would be the long way round to putting per-capability thresholds in Rust. A decision failure writes nothing and the graph's `Condition::Not(Truthy(...))` edge carries the turn to the generative node, per R6.

Tests: a node whose decision succeeds writes the label and the booleans; a node whose decision times out writes nothing and does not fail the execution; a threshold in config, not in code, changes which boolean is written.

### Task 5a: A capability's decision block travels on the catalog contract

**Files:** modify `common/proto/tools/v1/tools.proto` (`Tool` gains `string decision_json = 8;` — additive, the next free number after `status = 7`), `services/tool/src/declarative_seed.rs` (the operator's row gains an optional decision block), `services/tool/src/service.rs` (populate the new field), `services/tool/README.md`.

The block is its own field, never smuggled inside `input_schema_json`. Two declarations of the same arguments living in one string is how they drift apart, and the schema is what the arguments are validated against — the decision block is how they are *asked for*, which is a different concern with a different reader. Additive, so an engine that does not read it yet is unaffected; `buf breaking` must pass against the base SHA.

Per spec R4, every closed set gets an escape option and every capability gets an absolute `noul` beside the relative `choice`. Per the vendor's cookbook, option keys are the literal values the tool takes, so nothing maps a label back to an argument afterwards, and question text asks about meaning rather than naming the parameter. An argument the file does not declare closed makes its capability ineligible for the tree entirely (R1) — the row is rejected when the file is read, not degraded at runtime.

Tests: a row declaring an open argument as closed is refused at load; a row with no decision block loads exactly as today; the field round-trips through the catalog.

### Task 5b: The deciding node builds its questions from the live catalog

**Files:** modify `services/engine/src/executors/decide.rs`, `services/engine/README.md`.

Ends one service behind Task 5a, so each half builds and tests on its own. Reads the catalog's decision blocks, composes one request, applies its thresholds per R5, and dispatches.

Tests: a request for something outside a closed list selects the escape option rather than the nearest member; a capability whose absolute `noul` is low is not dispatched however peaked its `choice` is; a catalog with no decision blocks leaves every turn on today's path.

### Task 6: One speculative round

**Files:** modify `services/llm-router/src/budget.rs` (`MAX_QUESTIONS`), `services/llm-router/README.md`, `common/proto/llm_router/v1/llm_router.proto` (the budget comment).

Spec R8: the 64-question ceiling is ours, the vendor publishes none, and its own example sends 54. Raise it far enough that a tree of capabilities with all their arguments fits in one round, and keep the two real ceilings — 64k per request, 32k for state plus the longest question — enforced as they are today. The README records that what binds is tokens, not latency.

Tests: a request at the new ceiling passes; one over it is refused locally without spending a call; the combined state-plus-questions check still refuses a request that satisfies every field individually.

### Task 7: The invariant — adding a capability is a file edit

**Files:** create the test in `services/tool/` or `services/engine/` alongside the code it guards.

Spec R7. The test adds a capability to a fixture file with no subject in it, runs the load and the decision path, and asserts it is routable — with no Rust change anywhere in the diff. The existing grep-based gate test in `services/engine/src/executors/llm.rs` (`the_tool_use_policy_names_no_subject_and_no_tool`) is the precedent for how this repository enforces a rule of this kind; follow its shape rather than inventing a new one.

---

## Explicitly not doing

- **Regular expressions to find argument candidates**, even though the vendor documents the recipe and it works. Ruled out by the owner; consequence accepted in spec R1.
- **Template-rendered answers.** One generative call per turn is the floor (spec R3). A template with fallbacks is the pile of conditionals, moved into the renderer.
- **Two decision rounds.** Reversed after review; see spec R8. The re-ranking round the vendor documents is real but is not built now.
- **Extending `Condition` with numeric comparison.** Spec R5: it looks like generality and is in fact the doorway for per-capability thresholds into Rust.
- **Anything for open-ended arguments beyond what Task 3 measures.** If a capability needs a city name, it is a generative turn, and the tree is not where it belongs.
