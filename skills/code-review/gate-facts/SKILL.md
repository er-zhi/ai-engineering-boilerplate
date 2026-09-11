---
name: gate-facts
description: Use when a change relies on an external API, crate, model name, config key, or documented behavior, or when the user asks to verify facts, check official docs, or confirm something is real.
---

# Fact Check

Every external claim must trace to an official source. Training data goes stale; memory invents plausible APIs that do not exist.

Report findings using the template in [code-review](../SKILL.md).

## Verify Anything External

- Crate names, versions, and feature flags in `Cargo.toml`
- Function signatures, trait names, and return types from a dependency
- Model identifiers and provider endpoints (OpenRouter, DeepSeek)
- Environment variable and config key names a third party reads
- Postgres syntax, type behavior, and extension availability
- Protocol details — gRPC, HTTP status semantics, header names

Project-internal code needs no external source; read the repo instead.

## Sources, In Order

1. **Context7 MCP** — `resolve-library-id` then `query-docs`. First choice for any library, framework, SDK, or CLI.
2. **Official documentation or repository** — the vendor's own site, `docs.rs`, the project's GitHub.
3. **Web search** — only to locate an official page, never as the authority itself.

Blog posts, forum answers, and AI-generated summaries are leads to confirm, not evidence.

## Workflow

1. List every external claim the change depends on.
2. Look each one up. An unchecked claim is an unverified claim.
3. Record the source URL or Context7 library ID beside each claim.
4. Flag anything you could not confirm — do not soften it into a warning.

## Findings

| Severity | Condition |
|---|---|
| Critical | Claim contradicts the official source, or the API, crate, or model does not exist |
| Critical | Change depends on an external claim nobody verified |
| Warning | Source found but ambiguous, or the docs cover a different version than the one pinned |
| Suggestion | Verified, but a newer documented approach is now preferred |

## Reporting

Name the source, not just the verdict.

```
Critical: services/llm-router/README.md — model `deepseek/deepseek-chat` stopped serving
2026-07-24 per DeepSeek's API docs → use `deepseek/deepseek-v4-flash`
(openrouter.ai/deepseek/deepseek-v4-flash)

Verified: spider-rs is the crate behind the crawler (Context7 /spider-rs/spider)
```

That first finding is real — it came from running this gate against our own `llm-router` docs.

"Probably correct" is a Critical finding. Either it is confirmed or it is unverified.
