---
name: gate-evidence
description: Use when a change is claimed to work, be fixed, or be complete, or when the user asks for proof, verification, test output, or a screenshot before merge.
---

# Evidence

Whoever claims the code works shows it running. Evidence before assertions — "should work", "looks correct", and "compiles fine" are not evidence.

Report findings using the template in [code-review](../SKILL.md).

## What Counts

| Change | Evidence |
|---|---|
| Logic, mapper, pure function | `cargo test` output with the new test passing |
| Endpoint or handler | `curl` against the running service, request and response both shown |
| Service startup, Compose, Dockerfile | `docker compose up` log lines plus a `/health` response |
| Schema, entity, migration | `\d schema.table` or a query result showing the real column types |
| Schema isolation | Cross-schema `SELECT` returning `permission denied` |
| gRPC contract | Call output via `grpcurl` or an integration test |
| Dev HTML client, any UI | Screenshot of the rendered result |
| Bug fix | The failure reproduced first, then the same command passing |

## Rules

1. **Paste real output.** Copied terminal text or an attached image — never a description of what it printed.
2. **Show the changed behavior.** A green build proves compilation, not the feature. Exercise the specific path the diff touched.
3. **Include the command.** The reader must be able to rerun it verbatim.
4. **Reproduce failures first.** For a fix, the pre-fix error is half the evidence.
5. **Say so when you cannot.** "Not verified — no OpenRouter key locally" is an honest, acceptable finding. Silence is not.

## Keep It Cheap

One command, one paste. Evidence that takes a long setup won't get produced, so prefer:

```bash
cargo test -p crawler extract_main_content # narrow, seconds
curl -s localhost:8080/health              # already running
docker compose logs gateway | tail -5      # no restart needed
```

Reach for testcontainers or a full stack boot only when nothing smaller can show the behavior.

## Non-Blocking

This gate reports; it does not hold the merge hostage.

- Missing evidence on a **behavior change** → Critical
- Missing evidence on docs, comments, or renames → not applicable, skip the gate
- Evidence impossible in the local environment → Warning, with the reason stated
- Evidence provided but only tangential → Warning, name what is still unproven

Never block on evidence that the environment genuinely cannot produce. Record the gap and move on.

## Example

```
Verified: POST /api/crawl accepts pattern filters
$ curl -s -XPOST localhost:8080/api/crawl -H 'content-type: application/json' \
    -d '{"base_url":"https://example.com","exclude_patterns":["*/admin/*"]}'
{"job_id":"01JB...","status":"queued"}

Critical: services/crawler/src/denoise.rs — nav-stripping rewritten, no test run.
Expected `cargo test -p crawler denoise` output.
```
