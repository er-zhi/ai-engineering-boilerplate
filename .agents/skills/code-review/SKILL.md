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

The unit of review is a scope: a diff by default, or the whole repository on a first run or after a long gap. Every gate takes the scope in its prompt and reads only that.

1. Run the deterministic pre-review checks below. Stop and return their diagnostics to the implementing agent if any check fails; do not spend review-agent calls on mechanically invalid code.
2. Pick the gates that apply to the scope; skip the rest.
3. Note the wall-clock time before dispatch.
4. Dispatch **all** applicable gates at once, in parallel. Never run a gate serially.
5. Collect each gate's findings and how long it took.
6. Merge into the report below. Worst gate verdict wins.
7. Append one line to the run log — see [run-log.md](run-log.md).

## Deterministic Pre-review

Resolve the reviewed scope before running anything. A missing command or a version below the
minimum is `BLOCKED`, not a code failure. Use the exact base commit from the review scope for
compatibility checks; never infer it from whichever local branch happens to be checked out.

| Changed scope | Required checks |
|---|---|
| Rust, `Cargo.toml`, `Cargo.lock` | format, Clippy, nextest, doctest, coverage, cargo-deny, complexity |
| Python under `native/embedder-ane` | Ruff format/lint, ty, Python complexity |
| Protobuf or `buf.yaml` | Buf format, lint, breaking against the reviewed base SHA |
| Dockerfile or `compose.yaml` | Compose config and Hadolint |
| Markdown | typos and offline local-link checking |
| Any tracked source or configuration | Gitleaks |

Minimum supported toolchain: Rust 1.98.0, cargo-nextest 0.9.131, cargo-llvm-cov 0.9.1,
cargo-deny 0.20.2, Buf 1.73.0, Hadolint 2.15.1, Ruff 0.16.7, ty 0.0.80, Lizard 1.24.0,
typos-cli 1.50.1, Lychee 0.24.0, and Gitleaks 8.25.0. Check versions before the phases below.

Run direct tool commands in these phases. Commands within a phase may run in parallel. Phases run
in order, and Cargo commands that share the target directory run serially rather than waiting on
the same lock invisibly.

### Phase 1: cheap static checks

```bash
cargo fmt --all -- --check
docker compose config --quiet
hadolint services/*/Dockerfile
ruff format --check native/embedder-ane
ruff check native/embedder-ane
ty check --python native/embedder-ane/.venv/bin/python native/embedder-ane/server.py
uvx lizard -l rust -C 15 -T nloc=60 -a 6 -w common services
uvx lizard -l python -C 15 -T nloc=60 -a 6 -w native/embedder-ane/server.py
typos README.md common docs native/embedder-ane services .agents/skills/code-review
git ls-files -z '*.md' | xargs -0 lychee --offline
gitleaks dir . --config .gitleaks.toml --redact --no-banner
```

### Phase 2: contracts and dependencies

```bash
cargo deny check
buf format --diff --exit-code
buf lint
buf breaking --against '.git#ref=<review-base-sha>'
```

### Phase 3: shared Rust build state

```bash
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo nextest run --workspace --profile ci --status-level slow
cargo test --workspace --doc
cargo llvm-cov nextest --workspace --summary-only --fail-under-lines 90
```

Clippy performs compiler checking, so a separate `cargo check` only repeats work and is omitted.
`nextest` does not run doctests, so `cargo test --doc` remains mandatory. The 120-second suite
budget is a review warning; nextest's three-minute hard timeout is only a dead-run circuit breaker.
If the ANE virtual environment is absent, report Python type checking as blocked rather than
silently skipping it. Network-dependent external links and mutation testing are scheduled checks,
not pre-review blockers.

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
- Flag any gate over 60s and name what made it slow. Two gates are expected to exceed it: Second opinion (two external model calls, 5–10 min) and Facts on a full-repo scope (dozens of lookups). For those, compare against the run log, not the 60 s line.
- Gate agents need the host toolchain: `cargo`, `cargo-nextest`, `protoc`, Docker. A gate that finds one missing reports BLOCKED in one line and stops; it does not build inside a container or guess.

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
