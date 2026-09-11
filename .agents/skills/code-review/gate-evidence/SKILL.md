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
| Logic, mapper, pure function | `cargo nextest run` output with the new test passing |
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
4. **Say so when you cannot.** "Not verified — no OpenRouter key locally" is an honest, acceptable finding. Silence is not.

## Keep It Cheap

One command, one paste. Evidence that takes a long setup won't get produced, so prefer:

```bash
cargo nextest run -p crawler extract       # narrow, seconds
curl -s localhost:8080/health              # already running
docker compose logs --tail 5 gateway       # no restart needed
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
Verified: StartCrawl accepts scope filters through the gateway
$ curl -s -XPOST localhost:8080/crawler.v1.CrawlerService/StartCrawl \
    -H 'content-type: application/json' -H 'connect-protocol-version: 1' \
    -d '{"baseUrl":"https://example.com","scope":{"excludePatterns":["*/admin/*"]}}'
{"jobId":"job-1","status":"CRAWL_STATUS_QUEUED"}

Critical: services/crawler/src/extract.rs — nav-stripping rewritten, no test run.
Expected `cargo nextest run -p crawler extract` output.
```
