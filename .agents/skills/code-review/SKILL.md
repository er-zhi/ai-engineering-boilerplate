---
name: code-review
description: Use when reviewing a diff, PR, or completed feature in this boilerplate, or when the user asks for code review, parallel review, review gates, gate timings, or a pre-merge check.
---

# Code Review

Orchestrates the review gates. Each gate is one skill in this folder, one concern, run as its own agent loading only that skill plus the diff.

| Gate | Skill | Applies to |
|---|---|---|
| Architecture | [gate-architecture](gate-architecture/SKILL.md) | Service code, Dockerfile, Compose, migrations |
| Code quality | [gate-code-quality](gate-code-quality/SKILL.md) | Any Rust change |
| Types | [gate-common](gate-common/SKILL.md) | Entities, proto, shared code |
| Database | [gate-database](gate-database/SKILL.md) | Schema, entity fields, migrations |
| Testing | [gate-testing](gate-testing/SKILL.md) | Any Rust change — runs the suite, reports timing |
| Facts | [gate-facts](gate-facts/SKILL.md) | External APIs, crates, model names, docs claims |
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
| Claude Code | `Task` tool — one call per gate, all in a single message |
| Codex | Parallel agent calls in one turn |
| Either, as fallback | One `SKILL.md` read per gate in sequence, findings kept separate |

Two rules hold regardless of runtime:

- **One gate per agent.** A single agent reading all eight skills defeats the purpose — findings blur and the context budget goes to rules instead of the diff.
- **Never chain gates.** They share no state. Waiting for one before starting the next only adds latency.

If the runtime has no subagent mechanism, the sequential fallback still works — just keep each gate's pass isolated so one gate's conclusions don't color the next.

## Timing

Every gate reports its own duration. The review is only as fast as its slowest gate, so that gate is the only one worth optimizing.

- Record each gate's wall-clock seconds and put them in the report.
- Total is the **slowest gate**, not the sum — they ran in parallel. If total ≈ sum, they did not actually run in parallel; say so.
- Flag any gate over 60s, and name what made it slow.
- A gate that reads files is seconds. A gate that runs tests or calls another model is tens of seconds — that is expected, not a defect.

A single run cannot tell a chronically slow gate from a one-off spike, so every review appends a line to `logs/runs.jsonl` and the last 10 are kept. Read that history before optimizing anything — format, rotation, and the queries that rank gates by cost live in [run-log.md](run-log.md).

Recurring slow gates are a signal to fix the underlying cost, not to drop the gate: see [gate-testing](gate-testing/SKILL.md) for the test-suite case.

## Report Format

Every gate reports findings as `file:line — issue → fix`, grouped by severity. This is the only output template; gate skills do not define their own.

Three exceptions: Testing attaches its suite timing block, Evidence attaches commands and their real output instead of `file:line`, and Second opinion attributes each finding to the model that raised it.

```markdown
## Review: <scope>

**Verdict:** Approve | Approve with nits | Request changes
**Elapsed:** 34s (slowest gate: Testing 34s)

| Gate | Verdict | Critical | Warnings | Time |
|---|---|---|---|---|
| Architecture | ✅/❌ | N | N | 4s |
| Code quality | ✅/❌ | N | N | 5s |
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
- Only suggestions → **Approve with nits**
- Nothing found → **Approve**

Adding a gate: new folder `gate-<name>/SKILL.md` here, plus a row in both tables.
