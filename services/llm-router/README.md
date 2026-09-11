# LLM Router

Abstraction over LLM providers. Callers request a **quality tier**, never a provider or model name, and never hold provider credentials.

Provider: [OpenRouter](https://openrouter.ai/).

## Tiers

| Tier | Use case |
|---|---|
| `low` | Classification, keyword extraction |
| `medium` | Summaries, moderate reasoning |
| `high` | Complex or multi-step reasoning |

Each tier has a primary and a backup model, set in config; on timeout, rate limit, or 5xx the router retries the backup, then returns an error. Remapping a tier is a config change — callers are unaffected. `low` starts on DeepSeek V4 Flash — OpenRouter slug `deepseek/deepseek-v4-flash`, verified against [its model page](https://openrouter.ai/deepseek/deepseek-v4-flash).

Model slugs move. Re-verify against the [DeepSeek hub](https://openrouter.ai/deepseek) before pinning a new one.

## The Tier Contract Lives in `common`

[`common/src/llm.rs`](../../common/src/llm.rs) publishes what a tier accepts and promises, so a caller can check a request before sending it and this service enforces the same numbers:

- `SERVED_TIERS` — the tiers a caller may ask for.
- `limits(tier)` — `max_input_tokens`, `max_output_tokens`, and whether the tier reasons.
- `Sampling` — the settings a caller may set.
- `fit_to_limits` — rejects a prompt or an output request that exceeds the tier, and fills an unset `max_tokens` with the tier maximum, so a fallback never breaks the promise.

`DescribeTiers` returns the same contract over the wire for callers in other languages.

Limits belong to the contract, not to deployment: `.env` holds only which model serves each tier.

## Settings Follow the OpenAI Standard

`Sampling` carries the OpenAI chat-completions settings — `temperature`, `top_p`, `max_tokens`, `stop`, `frequency_penalty`, `presence_penalty`, `seed`, `logit_bias`, `logprobs`, `top_logprobs`, `response_format`. A setting the caller leaves unset is left out of the provider call entirely, so the model's own default applies.

Everything with a fixed set of values is an enum: `QualityTier`, `ResponseFormat`, `ReasoningMode`, and `FinishReason` — the caller sees what to expect rather than parsing provider strings.

## One Adapter per Provider

Integrating a provider means writing an adapter: a type implementing `Provider` in [`src/adapters/`](src/adapters/). The router itself knows only tiers, fallback, and the contract.

`openai_compatible` serves any provider speaking the OpenAI chat-completions API — OpenRouter today, another base URL tomorrow.

## Config

Keys: `OPENROUTER_API_KEY`, `OPENROUTER_BASE_URL`, and `LLM_<TIER>_PRIMARY` / `LLM_<TIER>_BACKUP` for `LOW`, `MEDIUM`, and `HIGH`. Values live in `.env` (gitignored); [`.env.example`](../../.env.example) holds the defaults.

A backup on the same vendor as its primary is not a backup — it shares the outage. Each tier's backup is therefore a different vendor.

This service is the only one given `OPENROUTER_API_KEY`. Callers reach it over gRPC and never see provider credentials, so the key stays out of Crawler and Gateway entirely.

## Reasoning Must Be Off for `low` and `medium`

Send `"reasoning": {"enabled": false}` on every `low` and `medium` request. Leave it on for `high` — that tier exists for it.

Reasoning models spend the completion budget on hidden reasoning tokens before emitting any content. On a one-word classification, `z-ai/glm-4.7-flash` consumed all 300 allowed tokens as `reasoning_tokens`, returned empty content, and stopped with `finish_reason: length`. With reasoning disabled the same prompt answered in 2 tokens. The failure bills normally and returns nothing, so it looks like a parsing bug rather than a config one.

Never treat an empty `content` as a model failure without checking `usage.completion_tokens_details.reasoning_tokens` first.

`{"reasoning": {"enabled": false}}` and `{"reasoning": {"effort": "none"}}` were both checked live against `deepseek/deepseek-v4-flash`: each answered with `reasoning_tokens: 0`.

## gRPC API

[`common/proto/llm_router.proto`](../../common/proto/llm_router.proto) — `Complete` and `DescribeTiers`. Streaming and tool calling are future work.

`POST /complete` exists for manual testing only. `GET /health` reports that this service is up; it does not call the provider, because a health check that spends money and depends on a third party stops being a health check. The key is verified once at startup instead, so a bad key fails the container rather than the first user request.

## Schema (`llm_router`)

Two tables, two lifetimes:

- `requests` — tier, model_used, used_backup, tokens_in, tokens_out, latency_ms, outcome, finish_reason, created_at. Statistics, kept for the long run.
- `request_payloads` — the full jsonb `sent` and `received` for one request. Kept 30 days, for taking a recent call apart; pruned after that.
