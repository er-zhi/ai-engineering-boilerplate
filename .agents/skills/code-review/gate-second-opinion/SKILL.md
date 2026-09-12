---
name: gate-second-opinion
description: Use when reviewing a diff in this boilerplate and independent judgment from other models is wanted, or when the user asks for a cross-model review, a Chinese model's or GLM's opinion, or a sanity check from Codex, Claude, or zcode.
---

# Second Opinion

Other models reviewing the same diff. Not another checklist — the other gates cover the rules. This one exists because models trained differently disagree in useful ways, and the disagreement is the signal.

When this gate runs, ask **two** models: the other Western agent, and GLM. Run them in parallel — they do not depend on each other. Report using the template in [code-review](../SKILL.md).

## Invocation

All three commands below were run successfully in this project. Each prints its answer to stdout.

**GLM via zcode — always run this one:**

```bash
zcode -p "Review services/crawler/src/. Judge the design, not the style. Where would this break?"
```

**Plus the other Western agent.** If you are Claude Code, call Codex:

```bash
codex exec --sandbox read-only --skip-git-repo-check "<same prompt>" < /dev/null
```

If you are Codex, call Claude Code:

```bash
claude -p --model sonnet "<same prompt>" < /dev/null
```

Send all of them the *same* prompt. Differing answers to one question are the point; differing answers to different questions prove nothing.

Feed Codex and Claude the diff rather than a list of paths — `codex review --uncommitted "<instructions>"` (also takes `--base <branch>` or `--commit <sha>`) and `git diff | claude -p "<instructions>"`. For `zcode`, name the changed paths in the prompt.

## Command Constraints

Each of these cost a failed run to find:

- **Tell every model, in the prompt, not to run skills, slash commands, or tools that write.** On the first full-repo run `zcode -p` found this project's `/code-review` skill and ran it recursively (six gates, including this one), took 626 s, and appended its own line to the run log. Open the prompt with: "Read the files and answer from reading. Do not run any skill, slash command, build, test, or review tool."
- **Launch both models with `run_in_background` and poll.** A foreground tool call caps at 600 s and a model that overruns it gets backgrounded late, which turns the two calls serial: 626 s + 300 s instead of max(626, 300). Budget 5–10 minutes of wall clock for this gate; it is always the slowest one, and that is expected.
- **Snapshot `git status --short` before launching and diff it right after each model returns.** Attribute a file change to a model only if it appears between that model's start and end and no other session was editing. On the first run the parent session was applying fixes while the models ran, and the report blamed zcode for edits it never made.
- **`zcode` accepts no flags alongside `-p`.** In v0.16.5, adding `--mode`, `--max-turns`, `--allowed-tools`, or `--json` makes it print help and exit without calling the model. Use the bare form only.
- **`zcode -p` runs in `yolo` permission mode** by default and offers no CLI flag to narrow it. Keep its prompts analysis-only, and run it on a clean tree so any stray write is visible in `git status`.
- **`codex exec` aborts** with `Not inside a trusted directory` outside a git repo — `--skip-git-repo-check` clears it.
- **Both `codex` and `claude` swallow inherited stdin** and append it to the prompt. Redirect `< /dev/null`.
- **A calling agent's own sandbox can block Codex** from starting its app-server client. Run it unsandboxed.

## Models

| Model | Command | Pinning |
|---|---|---|
| GLM 5.3 Flash | `zcode -p` | Z.ai settings — no headless `--model` flag; `/model` is TUI-only |
| GPT (gpt-5.6-sol) | `codex exec` | `-m <model>` |
| Claude | `claude -p` | `--model <alias>` |

To change the GLM model later, edit the Z.ai config rather than the command — `ANTHROPIC_BASE_URL` is `https://api.z.ai/api/anthropic` and the model IDs look like `glm-5.3-flash`. The commands in this skill stay the same.

## What to Ask For

Opinion, not compliance. Prompt for what a checklist cannot see:

- Is this design sound, or merely rule-abiding?
- What breaks first under load, concurrency, or failure?
- What did the author not consider?
- Is there a materially simpler approach?

Do not ask them to re-run our gates. Overlapping findings waste the call.

## Reporting

Attribute every finding to the model that raised it. Keep disagreement visible rather than averaging it away — a real run of the same prompt produced genuinely different answers:

```
GLM 5.3 Flash: Postgres serves as both vector and relational store with no pgvector
capacity planning — connection-pool saturation degrades the gRPC path as embeddings grow

Codex (gpt-5.6-sol): uncontrolled backpressure across crawl → embed → gRPC;
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
