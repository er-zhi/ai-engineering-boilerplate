---
name: gate-bug-fix
description: Use when a diff fixes a bug, crash, panic, wrong result, or bad input in this boilerplate, or when the user asks whether a fix reaches the root cause, or mentions a workaround, special case, hotfix, or regression.
---

# Bug Fix

A bug is a value or state the code let in and could not handle. The fix goes where the value got in, not where it blew up. A new `if` for the reported case at the crash site fixes that one input and leaves every input like it broken.

Where checks and limits belong is defined in [gate-code-quality](../gate-code-quality/SKILL.md#boundaries-and-limits); this gate checks that a fix follows it. Proof that the failure was reproduced belongs to [gate-evidence](../gate-evidence/SKILL.md). Report findings using the template in [code-review](../SKILL.md).

## Trace, Then Fix

1. Name the bad value or state, and the line where it failed.
2. Walk it back to where it entered the program: a request field, env var, file, network response, DB row, or the function that produced it. That point is the origin.
3. Fix at the origin, taking the first option that fits:
   - **Unrepresentable** — a type, enum, or newtype the bad value cannot be built into.
   - **Rejected at the boundary** — once, where it enters, with an error that names the rule.
   - **Bounded** — a named limit, so everything past the entry is known to fit.
4. Delete the downstream checks the origin fix made unreachable. A root-cause fix often shrinks the diff.
5. The regression test drives the origin with the whole class of input — empty, at the limit, one past it, malformed — not only the value from the report.

## Findings

| Severity | Symptom in the diff | Fix |
|---|---|---|
| Critical | New branch keyed on the reported value: a URL, id, string, or size | Name the rule that value breaks and enforce it at the origin |
| Critical | Guard or clamp added at the crash site, deep in the core | Move it to the boundary; the core takes the validated type |
| Critical | Error swallowed so the symptom disappears: matched by message text, `let _`, `.ok()`, or an early `return Ok(())` | Stop the input at the origin and report it there; propagate everything else |
| Critical | Retry, sleep, or longer timeout for a failure that repeats on every run | Fix the cause; retries are only for transient faults |
| Warning | The same check pasted at several call sites | One check at the single entry |
| Warning | Test covers only the exact input from the report | Test the boundary's rule at its edges |
| Warning | The diff does not name the origin | Trace it; a fix without a named cause is a guess |

## Example

Bug: storing a crawled page fails with `value too long for type character varying(2048)`.

Symptom patch: `PgPages::save` gains `if page.url.len() > 2048 { return Ok(()) }`. The page is dropped yet still counted as stored, the limit now lives in two places, and the next writer to `crawler.pages` hits the same error.

Root-cause fix: one `MAX_URL_CHARS = 2048` constant, enforced where URLs enter. `validate_base_url` rejects a longer base URL with `invalid_argument`, and the crawl drops a longer discovered link before counting it, logging why. `save` never sees such a URL and needs no check.
