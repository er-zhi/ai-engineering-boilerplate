---
name: code-review
description: Use when reviewing a diff, PR, or completed feature in this boilerplate, or when the user asks for code review, parallel review, review gates, gate timings, or a pre-merge check.
---

# Code Review

Orchestrates the review gates. Each gate is one skill in this folder, one concern, run as its own agent loading only that skill plus the diff.

| Gate | Skill | Applies to |
|---|---|---|
| Architecture | [gate-architecture](gate-architecture/SKILL.md) | Service code, Dockerfile, Compose, proto contracts |
| Code quality | [gate-code-quality](gate-code-quality/SKILL.md) | Any Rust change |
| Bug fix | [gate-bug-fix](gate-bug-fix/SKILL.md) | A diff that fixes a bug, crash, or wrong result |
| Types | [gate-common](gate-common/SKILL.md) | Entities, proto, shared code |
| Database | [gate-database](gate-database/SKILL.md) | Schema, entity fields, queries, migrations |
| Testing | [gate-testing](gate-testing/SKILL.md) | Any Rust change — runs the suite, reports timing |
| Facts | [gate-facts](gate-facts/SKILL.md) | External APIs, crates, model names, docs claims, how a tool is used |
| Evidence | [gate-evidence](gate-evidence/SKILL.md) | Any behavior change — proof it ran |
| Second opinion | [gate-second-opinion](gate-second-opinion/SKILL.md) | Architecture, concurrency, hard-to-reverse choices |

## Workflow

1. Pick the gates that apply to the diff; skip the rest.
2. Note the wall-clock time before dispatch.
3. Dispatch **all** applicable gates at once, in parallel. Never run a gate serially.
4. Collect each gate's findings and how long it took.
5. Merge into the report below. Worst gate verdict wins.
6. Append one line to the run log — see [run-log.md](run-log.md).

## Parallel Dispatch

Works in any agent runtime. Use whichever mechanism the host provides:

| Runtime | Mechanism |
|---|---|
| Claude Code | `Agent` tool (named `Task` in older versions) — one call per gate, all in a single message |
| Codex | Parallel agent calls in one turn |
| Either, as fallback | One `SKILL.md` read per gate in sequence, findings kept separate |

Two rules hold regardless of runtime:

- **One gate per agent.** A single agent reading every gate skill defeats the purpose — findings blur and the context budget goes to rules instead of the diff.
- **Never chain gates.** They share no state. Waiting for one before starting the next only adds latency.

If the runtime has no subagent mechanism, the sequential fallback still works — just keep each gate's pass isolated so one gate's conclusions don't color the next.

## Timing

Every gate reports its own wall-clock seconds, and the report shows them.

- Total is the **slowest gate**, not the sum — the gates ran in parallel. If total ≈ sum, they did not; say so.
- Flag any gate over 60s and name what made it slow.

One run cannot tell a chronically slow gate from a one-off spike. Read the history in [run-log.md](run-log.md) before optimizing anything.

## Report Format

Every gate reports findings as `file:line — issue → fix`, grouped by severity. This is the only output template; gate skills do not define their own.

Four exceptions: Testing attaches its suite timing block, Evidence attaches commands and their real output instead of `file:line`, Facts names the source beside each claim, and Second opinion attributes each finding to the model that raised it.

```markdown
## Review: <scope>

**Verdict:** Approve | Approve with nits | Request changes
**Elapsed:** 34s (slowest gate: Testing 34s)

| Gate | Verdict | Critical | Warnings | Time |
|---|---|---|---|---|
| Architecture | ✅/❌ | N | N | 4s |
| Code quality | ✅/❌ | N | N | 5s |
| Bug fix | ✅/❌ | N | N | 4s |
| Types | ✅/❌ | N | N | 3s |
| Database | ✅/❌ | N | N | 3s |
| Testing | ✅/❌ | N | N | 34s |
| Facts | ✅/❌ | N | N | 12s |
| Evidence | ✅/❌ | N | N | 8s |
| Second opinion | ✅/❌ | N | N | 22s |

### Critical
- [gate] `src/file.rs:42` — issue → fix

### Warnings
- [gate] `src/file.rs:10` — issue → fix

### Suggestions
- [gate] `src/file.rs:88` — improvement
```

## Verdict Rules

- Any Critical finding → **Request changes**
- Warnings or suggestions, no Critical → **Approve with nits**
- Nothing found → **Approve**

Adding a gate: new folder `gate-<name>/SKILL.md` here, plus a row in both tables.
