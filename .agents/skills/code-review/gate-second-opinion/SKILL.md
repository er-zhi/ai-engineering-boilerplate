---
name: gate-second-opinion
description: Use when reviewing a diff in this boilerplate and independent judgment from other models is wanted, or when the user asks for a cross-model review, a Chinese model's or GLM's opinion, or a sanity check from Codex, Claude, or zcode.
---

# Second Opinion

Other models reviewing the same diff. Not another checklist — the other gates cover the rules. This one exists because models trained differently disagree in useful ways, and the disagreement is the signal.

When this gate runs, ask **two independent model families** and run them in parallel. Prefer one of Codex or Claude plus GLM, choosing a model not responsible for the implementation. Report using the template in [code-review](../SKILL.md).

## Invocation

Use the installed CLIs' non-interactive, read-only modes. Each command prints its answer to stdout.

**GLM via zcode:**

```bash
zcode --mode plan --prompt "Read the diff and review its design. Do not run skills, builds, tests, or write tools."
```

**Add another model family.** If the implementing runtime is Claude Code, call Codex:

```bash
codex exec --sandbox read-only --ephemeral --skip-git-repo-check "<same prompt>" < /dev/null
```

If you are Codex, call Claude Code:

```bash
claude --print --restricted --permission-prompts none --disable-slash-commands --model sonnet "<same prompt>" < /dev/null
```

Send all of them the *same* prompt. Differing answers to one question are the point; differing answers to different questions prove nothing.

Give every model the same resolved scope: uncommitted changes, an exact base commit, or named paths. Let read-only tools inspect that scope; do not pipe repository contents through stdin when the CLI also treats stdin as prompt text.

## Command Constraints

- Tell every model not to invoke this review skill recursively or run builds and tests already owned by other gates.
- Use the CLI's read-only or plan mode in addition to the prompt. Prompt-only safety is insufficient.
- Launch both calls concurrently and measure wall-clock time as the slower call, not their sum.
- Snapshot `git status --short` before and after. A second-opinion agent must not modify the worktree.
- Redirect inherited stdin for Codex and Claude because both may append it to the prompt.
- Check each CLI's `--help` when its installed version changes; permission and headless flags are not stable contracts across vendors.

## Models

| Model | Command | Pinning |
|---|---|---|
| GLM | `zcode --prompt` | Z.ai settings; model selection is configured outside this command |
| GPT | `codex exec` | `--model <model>` when an explicit pin is required |
| Claude | `claude --print` | `--model <alias>` |

Record the actual model identifier reported by each runtime in the review result. Do not assume an alias still resolves to the model used by an earlier run.

## What to Ask For

Opinion, not compliance. Prompt for what a checklist cannot see:

- Is this design sound, or merely rule-abiding?
- What breaks first under load, concurrency, or failure?
- What did the author not consider?
- Is there a materially simpler approach?

Do not ask them to re-run our gates. Overlapping findings waste the call.

## Reporting

Attribute every finding to the model that raised it. Keep disagreement visible rather than averaging it away:

```
GLM: Postgres serves as both vector and relational store with no pgvector
capacity planning — connection-pool saturation degrades the gRPC path as embeddings grow

Codex: uncontrolled backpressure across crawl → embed → gRPC;
a slow LLM router stalls crawl workers until the pool is exhausted

Codex: suggests dropping the mapper module; disagree — it is the one place
entity ↔ proto conversion lives, which gate-common requires
```

## Severity

| Severity | Condition |
|---|---|
| Critical | Two models independently flag the same defect |
| Warning | One model raises a defect the gates missed, and it holds up on inspection |
| Suggestion | A genuinely different approach worth considering |
| Discard | Style nits, or findings another gate already owns |

A second opinion you disagree with is still worth reporting — say so and give the reason. Never defer to another model just because it is another model; both have far less context about this project than you do.

## When to Skip

Two model calls cost real time. Skip for renames, docs, formatting, and mechanical refactors. Worth it for architecture, concurrency, data modeling, and anything hard to reverse.
