# Latency, Round Two — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Cut the fixed cost of every generative call, and remove the fetch round from the common search turn — without putting another decision on the turn path.

**Architecture:** Three changes, ordered so the cheapest and safest lands first. A is configuration only. C is inside one service. B is conditional on C's result. Nothing here touches the agent graph's control flow, which the last round already changed.

**Tech Stack:** Rust 2024, Connect RPC, OpenRouter via `services/llm-router`, the `race` primitive in `services/tool`.

**Source:** measurements taken 2026-09-18 from this deployment's own audit tables, plus a live benchmark of candidate models. Every number below is measured here, not quoted from a vendor.

## What the measurements settled

Regression over 279 answered completions:

| tier | n | intercept | per output token | r (output) | r (input) |
|---|---|---|---|---|---|
| medium | 250 | 1720 ms | 23.8 ms | 0.56 | 0.09 |
| low | 29 | 2340 ms | 9.7 ms | 0.05 | 0.13 |

**Prompt size is a dead end** — input-token buckets of 858-893, 1610-2320 and 2409-3182 all sit at p50 ≈ 1900-1990 ms. **Tier switching is a dead end** — the low tier's intercept is *higher*.

**The fixed cost belongs to the provider, not the model.** OpenRouter spread those 250 medium calls over eight upstreams, because its default routing is price-weighted:

| provider | n | p50 |
|---|---|---|
| AtlasCloud | 165 | 2112 ms |
| StreamLake | 56 | 1963 ms |
| Baidu | 9 | 1755 ms |
| DeepInfra | 8 | **4680 ms** |
| SiliconFlow | 4 | 2738 ms |
| Alibaba | 3 | 1280 ms |

Turn anatomy, 124 executions:

| shape | share | components (p50) | total |
|---|---|---|---|
| no tool | 23% | Decide 150 + answer 1900 | ~2.1 s |
| search → answer | 35% | Decide 150 + llm 2100 + search 607 + answer 1866 | ~4.7 s |
| search → fetch → answer | 26% | + llm 3500 + fetch 781 | ~9.0 s |
| longer | 16% | | 11-66 s |

Tool execution is cheap (search 607 ms, fetch 781 ms). The turn is generative calls. And the node that emits a `web_fetch` call is the most expensive of all — 78 output tokens at ~24 ms each — because it spells out URLs the search returned one round earlier.

## Global Constraints

- Cargo is not on the default PATH: prefix every shell command with `export PATH="$HOME/.cargo/bin:$PATH"`.
- Strict lints: `unsafe_code = "forbid"`, clippy `all = deny`, `unwrap_used = "deny"`, `too_many_lines = "deny"`, `cognitive_complexity = "deny"` (threshold 20), `too_many_arguments = "deny"`.
- Provider credentials and model names exist only in `services/llm-router`. **No vendor or provider name may appear in Rust anywhere**, including this plan's Task A — the operator writes them in `.env`.
- `.agents/skills/code-review/gate-architecture/SKILL.md`, "Capabilities, Not Topics": no domain, topic or vendor subject in code, prompts or fixtures.
- A decision is advisory: one vendor, no tier, no fallback, and a failure must never lose a turn.
- **Verify the live chat before every commit** — `.superpowers/sdd/2026-09-17-system-one-latency/smoke-check.md`.

---

### Task A: Route by latency, and only to providers that honour our parameters

**Why.** Eight upstreams serve one model, chosen by price, with p50 from 1280 to 4680 ms for identical work. Worse: **4 of the 15 endpoints for our medium model do not support `response_format`**, and price-weighted routing can land on them — while this stack parses every agent reply as JSON. That is a live correctness hazard, not only a latency one.

**Files:**
- Modify: `services/llm-router/src/adapters/openai_compatible.rs` (request body)
- Modify: `services/llm-router/src/main.rs` (read the variable)
- Modify: `.env.example`, `services/llm-router/README.md`

**Steps:**
- [ ] Read `LLM_PROVIDER_ROUTING_JSON` at startup as an **opaque JSON object**, passed through verbatim as the request body's `provider` field. Absent → the body is exactly what it is today. Malformed → refuse to start, the same way a rejected key does; a silently ignored routing policy is worse than none.
- [ ] **No provider name and no routing field name appears in Rust.** The operator writes `{"sort":"latency","require_parameters":true}` in `.env`. This is what keeps the change on the right side of the credentials rule and the gate.
- [ ] Never send `allow_fallbacks: false` from code. If the operator sets it, that is their risk to take; a pinned provider having an outage would lose turns.
- [ ] Test: variable absent → no `provider` key in the body; variable present → passed through byte for byte. Assert against the existing request-body stub.
- [ ] After ~100 live calls, measure with the join that already exists: `llm_router.requests r JOIN llm_router.request_payloads p ON p.request_id = r.id`, grouped by `p.received->>'provider'`. If p50 has not moved, set `order` to the two fastest observed and measure again. If it still has not moved, **remove the variable and record that it did not pay** — a knob that does nothing is worse than no knob.
- [ ] Commit: `llm-router: provider routing is the operator's, and only providers honouring our parameters are eligible`

**Removes from the hot path:** ~300-600 ms per generative call, ~1.8 of them per turn → **0.5-1.0 s per turn**, plus it closes the `response_format` hazard. **Fails safe:** fallbacks stay enabled; an unreachable preferred provider routes as today. **Cost:** ~40 lines and one test.

---

### Task C: A search returns the pages it found

**Why.** 42% of tool turns spend a 3.5 s generative call whose entire output is "fetch these URLs", plus 0.8 s fetching them — one round after the search returned those exact URLs. The dependency is real, so the rounds cannot be parallelised; but the second round can be made unnecessary.

**Files:**
- Modify: `services/tool/src/service.rs` (`run_web_search`)
- Reuse: `services/tool/src/race.rs` and `services/tool/src/tools/web_fetch.rs`
- Modify: `services/engine/src/executors/llm.rs` (the rendering cap — see below)
- Modify: `services/tool/README.md` and the seeded tool description

**Steps:**
- [ ] **Fix the rendering cap first, or the pages never reach the model.** `MAX_TOOL_RESULT_CHARS = 4000` truncates the whole tool result. Input tokens cost ~0.29 ms each (measured, r = 0.09), so a larger result is nearly free in latency. Raise it to ~12 000, or cap per hit rather than per result. Test that two attached pages survive rendering.
- [ ] After the search returns, race fetches of the top `PREFETCH_CANDIDATES` (start at 4) hits through the existing `race` primitive with `take = 2` and a per-operation cap of ~1 s. Attach each success as `text` on its hit. A hit whose fetch fails keeps its snippet and gains nothing.
- [ ] `ensure_public_url` on every candidate before any request goes out, exactly as `run_web_fetch` does — the URLs come from a third-party search provider and are untrusted.
- [ ] Describe it as a capability, with no subject: "returns matching pages with a title, URL, snippet, and where a page could be read, its text".
- [ ] Test: a hit whose fetch fails renders exactly as today; a search whose every prefetch fails returns the snippet-only result — today's output — and never an error.
- [ ] Live check: a question that needed search → fetch yesterday should now complete as `llm → tool → llm`.
- [ ] Commit: `tool: a search returns the pages it found, so the agent does not spend a round asking for them`

**Removes from the hot path:** ~4.3 s on 42% of tool turns; costs ~0.5-0.9 s of prefetch on the turns that would not have fetched. Net ≈ **1.3 s per turn on average, ~4 s on the slow shape**. **Fails safe:** every prefetch failure degrades to exactly today's behaviour. **Cost:** ~150 lines plus tests, all inside the tool service.

---

### Task B (conditional): shorter fetch calls

Only if fetch rounds remain common after C. Cap the URL list in the tool's description at 3 and measure that node's `tokens_out`: 78 → ~35 tokens is ~1 s per fetch round. **Do not** build index-based URL references — that needs argument resolution in the engine, for a gain C should already have taken.

---

### Task M: Measure again

Rerun the per-shape and per-provider queries after A and C are live, and record them in the plan's workspace. The number worth publishing is the **whole search turn**, Decide plus every node: today ~4.7 s, or ~9.0 s when a fetch is needed.

---

## On changing the model — done, and it was the biggest single win

Benchmarked manually on 2026-09-18 by replaying three real prompts from
`llm_router.request_payloads`, with provider routing pinned to
`{"sort":"latency","require_parameters":true}` so the comparison is between
models rather than between whichever upstream price-weighted routing picked.
The three prompts are the three roles an agent node actually plays: choose a
tool when search results hold no answer; choose a tool from a bare question;
answer from a page that does hold the answer.

**Result, 12 runs per model per role:**

| model | pick a tool | answer from the page |
|---|---|---|
| `deepseek/deepseek-v4.1-flash` | **12/12 correct, p50 828 ms** | **12/12 correct, p50 876 ms** |
| `deepseek/deepseek-v3.2` (was medium) | **6/12 correct**, p50 2529 ms | 12/12 correct, p50 1714 ms |
| `deepseek/deepseek-v4-flash` | 12/12, p50 1822 ms | 9/12, p50 3461 ms |

The model we were running **chose the wrong tool half the time** and was three
times slower doing it. That was invisible because nothing measured it.

Also rejected, with reasons, so nobody re-tests them:

| model | why not |
|---|---|
| `amazon/nova-micro-v1` | fastest of all at 665 ms and **invents data** — answered with a temperature that appears nowhere in the tool results |
| `ibm-granite/granite-4.0-h-micro` | 16x cheaper, correct on one role, but answers a two-part question with one part and picks the knowledge base for general knowledge |
| `anthropic/claude-haiku-4.5` | does not hold the JSON envelope this prompt requires |
| `google/gemini-3.8-flash` | the only model correct on all three roles, but 2-4x slower than v4.1-flash and 5x the input price |
| `qwen/qwen3.8-flash`, `ibm-granite/granite-4.2-8b`, `cohere/command-r7b`, `inception/mercury-2.5`, `qwen3-30b-a3b` | broken JSON, literal `<argument name>` placeholders, empty replies, or 20-27 s latency |

**Applied:** `LLM_MEDIUM_PRIMARY=deepseek/deepseek-v4.1-flash`, with the previous
model kept as `LLM_MEDIUM_BACKUP`. Live check after the switch: a two-part
question answered "Capital: Tokyo. Today: cloudy, 79°F, feels like 71°F" — both
halves, both grounded in the fetched page.

**The lesson worth keeping:** speed and correctness were anti-correlated among
the cheap models, and the fast ones were fast because they were weak. Only a
benchmark on the real prompts, graded against the evidence the model was given,
separated them. This is the argument for an eval service.

## Explicitly not doing

- **Streaming the answer.** The answer node is ~1.9 s of which ~1.5 s is time to first token, and the model must emit `{"tool_call":null,"reply":"` before any answer text — so you cannot know it is not a tool call until roughly eight tokens in, and you would need incremental JSON parsing. The gain is ~400 ms of perceived latency on one node; the cost is a server-streaming RPC in llm-router, a side channel out of an engine node that today only exists at checkpoint commit, a Chat event kind, and the frontend. Four services. Revisit when answers routinely exceed ~50 tokens.
- **A typed "is this enough to answer?" decision after the first tool result.** It saves nothing when the answer is yes, and still needs a generative call to compose the fetch when it is no. Task C removes the round instead of deciding it faster.
- **Parallel tool calls, or `FanOut` in the agent graph.** The machinery already exists twice — `executors/tool.rs` runs an array of calls through `FuturesUnordered`, and `FanOut`/`FanIn` are real in `engine-core`. Neither helps: the observed rounds are *dependent* (the fetch needs the search's URLs), and genuinely independent themes are already split into parallel topics by Chat.
- **Cutting prompt size, or switching tier.** r = 0.09 against input tokens; the low tier's intercept is higher than medium's.
- **Pinning a provider in code, or `allow_fallbacks: false`.** Vendor names belong to the operator, and a pinned provider's outage would lose turns.
