---
name: gate-facts
description: Use when a change relies on an external API, crate, model name, config key, or documented behavior, or works around a tool instead of using it the documented way, or when the user asks to verify facts, check official docs, confirm something is real, or mentions a workaround, hack, or shell script.
---

# Fact Check

Every external claim must trace to an official source, and every tool is used the way its official docs describe. Training data goes stale; memory invents plausible APIs that do not exist.

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

## Documented Way, Not a Workaround

When a tool, crate, or image documents a way to do something, the change uses that way. A workaround is allowed only when the official docs offer none, and then the change links the docs that show it.

| Workaround | Documented way |
|---|---|
| Shell script or Makefile wrapping a tool | The tool's own command; for Docker and Compose, see [gate-architecture](../gate-architecture/SKILL.md#docker--compose) |
| `sleep` until something is ready | The tool's readiness check: Compose `depends_on` with `condition: service_healthy`, a testcontainers wait strategy |
| Retry loop around a client that has retry and timeout settings | Those settings |
| Forked or vendored copy of a crate, made to change its behavior | The crate's feature flag or config; otherwise fix it upstream |

## Budget

A diff usually has only a handful of external claims. For a full-repository review, split independent source groups so crate and container facts, model/provider facts, and PostgreSQL/Compose facts can be checked concurrently. Batch lookups that share an official index and do not re-check claims whose pinned source and version are unchanged.

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
| Critical | A workaround where the official docs describe a standard way |
| Warning | A workaround the docs leave no alternative to, but the change does not link them |
| Warning | Source found but ambiguous, or the docs cover a different version than the one pinned |
| Suggestion | Verified, but a newer documented approach is now preferred |

## Reporting

Name the source, not just the verdict.

```
Critical: services/llm-router/README.md — configured model slug is absent from
OpenRouter's current model index → select an available slug and verify its capabilities
(official model-page URL)

Verified: spider-rs is the crate behind the crawler (Context7 /spider-rs/spider)
```

"Probably correct" is a Critical finding. Either it is confirmed or it is unverified.
