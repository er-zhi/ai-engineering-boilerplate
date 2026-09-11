# LLM Router

Abstraction over LLM providers. Callers request a **quality tier**, never a provider or model name, and never hold provider credentials.

Provider: [OpenRouter](https://openrouter.ai/).

## Tiers

| Tier | Use case |
|---|---|
| `low` | Classification, keyword extraction |
| `medium` | Summaries, moderate reasoning |
| `high` | Complex or multi-step reasoning |

Every tier starts on DeepSeek V4 Flash — OpenRouter slug `deepseek/deepseek-v4-flash`, verified against [its model page](https://openrouter.ai/deepseek/deepseek-v4-flash). Each tier has a primary and a backup model; on timeout, rate limit, or 5xx the router retries the backup, then returns an error. Remapping a tier is a config change — callers are unaffected.

Model slugs move. Re-verify against the [DeepSeek hub](https://openrouter.ai/deepseek) before pinning a new one.

## Config

Values live in `.env` (gitignored); see [`.env.example`](../../.env.example) for the full list.

```env
OPENROUTER_API_KEY=sk-or-v1-...
OPENROUTER_BASE_URL=https://openrouter.ai/api/v1
LLM_LOW_PRIMARY=deepseek/deepseek-v4-flash
LLM_LOW_BACKUP=z-ai/glm-4.7-flash
# same pattern for MEDIUM and HIGH
```

A backup on the same vendor as its primary is not a backup — it shares the outage. Each tier's backup is therefore a different vendor.

This service is the only one given `OPENROUTER_API_KEY`. Callers reach it over gRPC and never see provider credentials, so the key stays out of Crawler and Gateway entirely.

## Reasoning Must Be Off for `low` and `medium`

Send `"reasoning": {"enabled": false}` on every `low` and `medium` request. Leave it on for `high` — that tier exists for it.

Reasoning models spend the completion budget on hidden reasoning tokens before emitting any content. On a one-word classification, `z-ai/glm-4.7-flash` consumed all 300 allowed tokens as `reasoning_tokens`, returned empty content, and stopped with `finish_reason: length`. With reasoning disabled the same prompt answered in 2 tokens. The failure bills normally and returns nothing, so it looks like a parsing bug rather than a config one.

Never treat an empty `content` as a model failure without checking `usage.completion_tokens_details.reasoning_tokens` first.

## gRPC API

`common/proto/llm_router.proto` — `Complete(CompleteRequest) → CompleteResponse`. Streaming is future work.

```protobuf
enum QualityTierEnum { LOW = 0; MEDIUM = 1; HIGH = 2; }

message CompleteRequest {
  QualityTierEnum tier = 1;
  string system_prompt = 2;
  string user_prompt = 3;
  float temperature = 4;
  int32 max_tokens = 5;
}

message CompleteResponse {
  string content = 1;
  string model_used = 2;
  int32 tokens_in = 3;
  int32 tokens_out = 4;
  bool used_backup = 5;
}
```

`POST /complete` exists for manual testing only. `GET /health` also checks OpenRouter connectivity.

## Schema (`llm_router`)

- `requests` — id, tier, model_used, tokens_in, tokens_out, latency_ms, status, created_at

Usage metrics only. Prompts and responses are not stored.

## Adding a Provider

Add the client, map tiers to its models in config. Calling services keep sending `QualityTierEnum`.
