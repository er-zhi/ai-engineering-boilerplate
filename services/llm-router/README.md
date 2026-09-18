# LLM Router

Abstraction over LLM providers. Callers request a **quality tier**, never a provider or model name, and never hold provider credentials.

Two classes of model are served, each with its own contract, its own adapter, and its own tables:

| Class | RPC service | What you send | What you get back |
|---|---|---|---|
| Text completion | `LlmRouterService` | a system and user prompt | generated text |
| Structured decision | `SystemOneService` | a state and typed questions | a typed, calibrated answer per question |

Providers: [OpenRouter](https://openrouter.ai/) for completion, [TypeSafe AI](https://typesafe.ai/) for decisions.

## Tiers

| Tier | Use case |
|---|---|
| `low` | Classification, keyword extraction |
| `medium` | Summaries, moderate reasoning |
| `high` | Complex or multi-step reasoning |

Each tier has a primary and a backup model, set in config; on timeout, rate limit, or 5xx the router pauses 250 ms and tries the backup, then returns an error. Remapping a tier is a config change — callers are unaffected. `low` starts on DeepSeek V4 Flash — OpenRouter slug `deepseek/deepseek-v4-flash`, verified against [its model page](https://openrouter.ai/deepseek/deepseek-v4-flash).

Model slugs move. Re-verify against the [DeepSeek hub](https://openrouter.ai/deepseek) before pinning a new one.

## The Tier Contract

[`src/llm.rs`](src/llm.rs) holds what a tier accepts and promises, and this service enforces those numbers on every request:

- `SERVED_TIERS` — the tiers a caller may ask for.
- `limits(tier)` — `max_input_tokens`, `max_output_tokens`, and whether the tier reasons.
- `fit_to_limits` — rejects a prompt or an output request that exceeds the tier, and fills an unset `max_tokens` with the tier maximum, so a fallback never breaks the promise.

`DescribeTiers` returns the same contract over the wire for callers in other languages.

Limits belong to the contract, not to deployment: `.env` holds only which model serves each tier.

## Settings Follow the OpenAI Standard

The proto `Sampling` message carries the OpenAI chat-completions settings — `temperature`, `top_p`, `max_tokens`, `stop`, `frequency_penalty`, `presence_penalty`, `seed`, `logit_bias`, `logprobs`, `top_logprobs`, `response_format`. A setting the caller leaves unset is left out of the provider call entirely, so the model's own default applies.

Everything with a fixed set of values is an enum: `QualityTier`, `ResponseFormat`, `ReasoningMode`, and `FinishReason` — the caller sees what to expect rather than parsing provider strings.

## One Adapter per Provider

Integrating a provider means writing an adapter: a type implementing `Provider` in [`src/adapters/`](src/adapters/). The router itself knows only tiers, fallback, and the contract.

Each adapter is named after the standard it speaks, not the vendor that happens to serve it today:

- `openai_compatible` — any provider speaking the OpenAI chat-completions API. OpenRouter today, another base URL tomorrow.
- `system_one` — any provider speaking the [System One](https://docs.typesafe.ai/api) API. TypeSafe AI today.

Environment variables stay vendor-named (`OPENROUTER_*`, `TYPESAFE_AI_*`), because a key belongs to a vendor rather than to a standard.

## Config

Completion: `OPENROUTER_API_KEY`, `OPENROUTER_BASE_URL`, and `LLM_<TIER>_PRIMARY` / `LLM_<TIER>_BACKUP` for `LOW`, `MEDIUM`, and `HIGH`.

Decisions: `TYPESAFE_AI_API_KEY`, and optionally `TYPESAFE_AI_BASE_URL` (defaults to `https://api.typesafe.ai`) and `TYPESAFE_AI_MODEL` (defaults to `jev-latest`).

Also optional, completion only: `LLM_PROVIDER_ROUTING_JSON`, a JSON object that is **opaque to this service** — it is read only far enough to confirm it parses as an object, then forwarded verbatim as the `provider` field of every completion request. Left unset, the request body is unaffected. Set to something that is not valid JSON, or valid JSON that is not an object, and the container fails at startup, exactly as a rejected `OPENROUTER_API_KEY` does — a routing policy silently ignored would be worse than none. See [`.env.example`](../../.env.example) for the exact shape.

Values live in `.env` (gitignored); [`.env.example`](../../.env.example) holds the defaults.

`TYPESAFE_AI_API_KEY` is optional in one specific sense: **leave it unset** and the service starts normally with completion unaffected, while `Decide` and `DescribeModels` answer a clear "not configured" error when called. **Set it to something the provider rejects** and the container fails at startup, exactly as a rejected `OPENROUTER_API_KEY` does — a wrong credential is a deployment mistake worth failing loudly, not a feature to silently drop. So leave it empty or make it right; do not leave a placeholder in it.

Each tier uses models from different publishers to reduce correlated model-specific failures. Both still depend on OpenRouter, so this is fallback within the provider, not protection from a complete OpenRouter outage.

This service is the only one given `OPENROUTER_API_KEY`. Callers reach it over gRPC and never see provider credentials, so the key stays out of Crawler and Gateway entirely.

## Reasoning Must Be Off for `low` and `medium`

Send `"reasoning": {"effort": "none"}` on every `low` and `medium` request — the form [OpenRouter documents](https://openrouter.ai/docs/use-cases/reasoning-tokens) for disabling reasoning. Leave it on for `high` — that tier exists for it.

Reasoning models can spend the completion budget on hidden reasoning tokens before emitting content. For `low` and `medium`, the adapter therefore sends the documented `{"reasoning": {"effort": "none"}}` form. When a response has empty content, inspect `usage.completion_tokens_details.reasoning_tokens` before classifying the failure.

## Structured Decisions: the System One Class

A System One model does not generate text. It reads a **state** and answers **typed questions** about it with calibrated probabilities, so the decision lands in your code as a number rather than as prose to parse. Use it where an `if` needs a judgement it cannot compute: routing, triage, gating, ranking. Use `Complete` when you actually want words.

Two callers use it today: Chat's `intent.rs` calls `Decide` once per turn to route it onto a topic, and reaches for `Complete` only to write out the several `title`/`question` pairs a multi-theme split needs; Tool's `validate_tool` calls `Decide` against a set of per-criterion questions and composes a refusal in Rust from whichever answers came back false — it calls `Complete` nowhere in that path.

[`SystemOneService`](../../common/proto/llm_router/v1/llm_router.proto) serves the class:

- `Decide` — one state, one or more questions, one answer per question.
- `DescribeModels` — which models the configured provider serves, **and the budget every `Decide` must fit**. It is the class's counterpart to `DescribeTiers`: a caller in any language can read the limits and check a request before paying to have it refused. The numbers below are rendered from the same constants the validator enforces, so the two cannot drift.

### Three Question Types

Each question carries an `id` you choose, `instructions`, and the fields of its type. Answers come back under the same ids.

| Type | Ask it when | Answer |
|---|---|---|
| `noul` | the question is yes/no | `noul`: probability of yes, 0 to 1 |
| `choice` | one option out of a set you define | `choice`, `probabilities` per option, `confidence` |
| `score` | a rating along ordered levels you define | `score` weighted across levels, `legend`, `probabilities`, `confidence` |

`noul` may describe what a yes and a no mean (`whenTrue` / `whenFalse`); both are optional. `choice` takes 1 to 255 options, and an option may omit its description. `score` takes 2 to 10 levels, ordered from lowest to highest.

Those two ceilings are the vendor's, published on the [choice](https://docs.typesafe.ai/primitives/choice) and [score](https://docs.typesafe.ai/primitives/score) pages. `Decide` enforces them itself, so an eleventh level is a free local rejection rather than a paid provider 422.

Options are **repeated, not maps**: the vendor's own API takes choice criteria as a map keyed by name and evaluates each question independently and in isolation, so option order carries no meaning to it; `repeated` is used because it is a stable wire shape and a protobuf map has none. `score` levels are different: the vendor keeps them as an ordered array end to end too, evaluated lowest to highest, so their order is the one place it actually matters.

### Ask Everything at Once

One call carries up to 64 questions, answered against the same state in a single provider round trip. That ceiling is ours, not the vendor's — the vendor caps tokens, not question count — and it is there because a request needing more than 64 questions at once is asking about too much in one call; the token budget below is the real constraint. TypeSafe calls this pattern [speculative fan-out](https://docs.typesafe.ai/patterns/fan-out): ask the questions a branch *might* need, then let your code pick which answers matter. A second question is far cheaper than a second call.

```bash
curl -X POST llm-router:8083/llm_router.v1.SystemOneService/Decide \
  -H 'content-type: application/json' -H 'connect-protocol-version: 1' \
  -d '{
    "state": "Help! My payouts have been failing for 3 days.",
    "questions": [
      {"id": "is_urgent",
       "instructions": "Does this convey urgency?",
       "noul": {"whenTrue": "Explicitly time-sensitive", "whenFalse": "No urgency expressed"}},
      {"id": "department",
       "instructions": "Which team should handle this?",
       "choice": {"options": [
         {"name": "billing",   "description": "Payments, invoicing, refunds"},
         {"name": "technical", "description": "Bugs, outages, integrations"},
         {"name": "sales",     "description": "Pricing, upgrades, new accounts"}]}},
      {"id": "frustration",
       "instructions": "How frustrated is the customer?",
       "score": {"levels": ["Calm", "Frustrated", "Very angry"]}}
    ]
  }'
```

```json
{
  "modelUsed": "jev-1.13.0",
  "answers": [
    {"id": "is_urgent", "noul": {"noul": 0.95}},
    {"id": "department", "choice": {
      "choice": "billing",
      "probabilities": {"billing": 0.88, "technical": 0.12, "sales": 0.0},
      "confidence": 0.81}},
    {"id": "frustration", "score": {
      "score": 1.04,
      "legend": {"0": "Calm", "1": "Frustrated", "2": "Very angry"},
      "probabilities": {"0": 0.0, "1": 0.96, "2": 0.04},
      "confidence": 0.94}}
  ],
  "tokensIn": 426,
  "tokensOut": 73
}
```

`modelUsed` is the version that actually answered, not the alias configured — `jev-latest` resolves to something like `jev-1.13.0`, so a row in `decisions` names the model that produced it even after the alias moves.

`state` is any JSON — a string for plain text, an object or array for a chat log, a record, or the current state of your application. It crosses the wire as `google.protobuf.Value`, which costs two things worth knowing, and the same applies to `instructions`:

- **Every number is a double.** `{"days": 3}` reaches the provider as `{"days": 3.0}`, and an id beyond 2^53 loses precision outright. Send anything that must stay an exact integer as a string.
- **An object's keys arrive sorted.** A `Value` object is a map, and this workspace's `serde_json` has no `preserve_order`, so the model sees the keys alphabetised rather than as you wrote them. Where order carries meaning — a transcript, a sequence of events — send an **array**, which keeps it.

Ordering only changes an answer for `score` levels, which stay an ordered array end to end — including on the vendor's side — and are evaluated lowest to highest; that is `repeated` for exactly this reason. Questions and choice options are `repeated` too, but for wire stability: the vendor takes them as maps keyed by id and evaluates each question independently and in isolation, so their order does not reach it.

### Confidence Is the Second Axis

`probabilities` says *what*; `confidence` says *whether to act on it*. It collapses the shape of the distribution into one number: concentrated means certain, flat means the model cannot separate the options. Only `choice` and `score` carry it — a `noul` value already is its probability.

Threshold it by what a wrong answer costs, not by one global number: show the wrong read-only screen at 0.6, ask a human before approving a transfer at 0.9, and route anything under 0.5 to a person rather than guessing. See [Confidence](https://docs.typesafe.ai/confidence) for the reasoning.

### No Tiers, No Fallback

One vendor serves this class today, so a decision runs on one model and a failure is returned rather than retried on a second. Tiers and primary/backup exist on the completion side because two vendors serve it; when a second System One provider appears, the same `Tiers` shape applies here.

### The Budget

`Decide` rejects a request that exceeds any of these before spending anything, naming the limit and the question it refused. `DescribeModels` publishes the same numbers. Every row but the last two bounds one field on its own; a request can satisfy every one of those individually and still be refused, because the last two rows bound `state` plus questions together, not each field independently: one over `state` plus the single largest question, the other over `state` plus every question in the call summed.

| Limit | Value |
|---|---|
| questions in one call | 64 |
| question id | 64 bytes |
| `instructions` per question | 8 KiB |
| `state` | 48 KiB |
| `choice` options | 1 to 255 |
| option name | 64 bytes |
| option description | 1 KiB |
| `score` levels | 2 to 10 |
| level text, and each noul criterion | 1 KiB |
| `state` plus the largest question, combined | 64 KiB |
| `state` plus every question, combined | 128 KiB |

Shape is checked too: an empty question list, a duplicate or empty id, a question with no type, and a `state` or `instructions` the caller never set are all refused locally rather than sent and billed.

### What a Failure Means

The three outcomes are distinct on purpose, because they call for different reactions:

- **`invalid_argument`** — the request is wrong, and it is the caller's to fix. Either this service refused it against the budget above, or the provider did on its shape (400, 409, 413, 422). The provider's own words come back with it. Retrying unchanged will not help.
- **`unavailable`** — a transient fault: a 429, a 529, a timeout. The caller decides whether to retry. The provider's `retry-after` is not passed through, so back off on your own schedule rather than reading one off the error.
- **`internal`** — this deployment's problem, not the caller's: a revoked or exhausted key (401, 402, 403), a base URL that does not resolve (404), or a reply this service could not read. A caller cannot fix any of these by changing its request, so it is never told that it could. The provider's reply is in `decision_payloads` for whoever operates the service.

## No Embeddings Here

This service handles text completion and structured decisions only. Knowledge Base calls the [native embedder](../knowledge-base/README.md#embedding-native-apple-silicon-only) directly, so embedding needs no provider credential or model route here.

## gRPC API

[`common/proto/llm_router/v1/llm_router.proto`](../../common/proto/llm_router/v1/llm_router.proto) defines the supported API: `Complete` and `DescribeTiers` on `LlmRouterService`, `Decide` and `DescribeModels` on `SystemOneService`. Streaming and tool calling are not supported.

Every RPC answers at the Connect path, JSON body, no client library needed:

```bash
curl -X POST llm-router:8083/llm_router.v1.LlmRouterService/Complete \
  -H 'content-type: application/json' -H 'connect-protocol-version: 1' \
  -d '{"tier":"QUALITY_TIER_LOW","systemPrompt":"Reply with exactly one word.","userPrompt":"Say the word: ok","sampling":{"maxTokens":20}}'
```

`GET /health` reports that this service is up; it does not call a provider, because a health check that spends money and depends on a third party stops being a health check. Each configured key is verified once at startup instead — `OPENROUTER_API_KEY` against OpenRouter's `/key`, `TYPESAFE_AI_API_KEY` against TypeSafe's `/v1/models` — so a bad key fails the container rather than the first user request. **Any 4xx counts as refusal**, not just a 401: a 403 or a 404 from a mistyped base URL is a deployment mistake, and treating it as "provider unreachable" would let the service start and fail every call instead. Only a provider that cannot be reached at all — a transport error — logs a warning and starts anyway, so a provider outage never crash-loops the container.

## Schema (`llm_router`)

Each class keeps its own pair of tables, because a decision has no tier, no backup, and no finish reason, and a completion has no question count. Every pair has the same two lifetimes: statistics for the long run, payloads for as long as a post-mortem needs them.

Completion:

- `requests` — tier, model_used, used_backup, tokens_in, tokens_out, latency_ms, outcome, finish_reason, created_at. `tier`, `outcome`, and `finish_reason` are Rust enums stored as short strings.
- `request_payloads` — the full jsonb `sent` and `received` for one request.

Decisions:

- `decisions` — model_used, questions (how many the call carried), tokens_in, tokens_out, latency_ms, outcome, created_at.
- `decision_payloads` — the same jsonb `sent` and `received`.

Both rows of a call are written in one transaction.

### All Four Are Partitioned by Month

Every one of these tables grows with time and is never updated, so all four are `PARTITION BY RANGE (created_at)` with monthly children — the same shape as [`chat.events`](../chat/README.md) and `engine.execution_events`, and for the same reason: retention on a log-shaped table should be dropping a partition, not scanning it for expired rows. The primary key is `(created_at, id)`, since a partitioned table's key must carry its partition key.

An hourly pass in [`src/partition.rs`](src/partition.rs) opens the coming month for all four parents and drops payload partitions that fell out of the window. Retention is therefore **a month, not exactly 30 days**: a payload written in month M is dropped once M+2 opens, so it survives at least as long as the month after it — **28 days** when that month is February, 29 in a leap year — and at most **62**, when it was written on the first of a 31-day month followed by another. That is the price of dropping partitions instead of deleting rows, and it is why there is no `expires_at` column any more — nothing read it once retention became a `DROP`.

Statistics are never dropped. Only the two payload tables have a retention window.

Because a partitioned parent cannot be expressed through the SeaORM registry glob, the entities live in [`src/audit/`](src/audit/) rather than `src/entity/`, each carrying its own literal `CREATE TABLE … PARTITION BY RANGE` statement. A parent that already exists as a plain table silently swallows `CREATE TABLE IF NOT EXISTS … PARTITION BY RANGE`, so startup checks `pg_get_partkeydef` first and refuses to run — naming every offending table — rather than coming up with no retention path at all.
