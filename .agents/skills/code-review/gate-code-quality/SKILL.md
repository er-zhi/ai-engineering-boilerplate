---
name: gate-code-quality
description: Use when writing or reviewing Rust in this boilerplate, or when the user mentions NASA rules, Linux kernel style, input validation, limits, special cases, comments, naming, file size, over-engineering, or what can be deleted.
---

# Code Quality

Durable code is simple, explicit, and self-explaining. Names replace comments, small files replace abstraction layers, and input is checked once at the boundary so the code behind it stays thin.

Report findings using the template in [code-review](../SKILL.md). For deletion-only passes, use [ponytail.md](ponytail.md).

## Style Rules

1. **No comments** — the only exception is one summary line at line 1 of a file: `// Fetches and stores page embeddings.` States what the file does, not how. `///` doc comments, test comments, and comments explaining an `allow` count too. When a comment seems needed, move its meaning into a name: a constant, a function, or a variable.
2. **Self-documenting names** — a reader needs no explanation beyond the identifier.
3. **Minimal files** — under ~200 lines; split only when a file does two distinct jobs. Functions do one thing, in under ~60 lines.
4. **No dead weight** — no unused imports, no speculative abstractions, no wrapper that only delegates.

## Boundaries and Limits

Modeled on how the Linux kernel treats a system call: each argument from user space is checked at entry, a bad one fails right there with an error code such as `EINVAL` or `ENAMETOOLONG`, and sizes have hard limits such as `PATH_MAX`. The code behind the entry trusts what it receives.

1. **Check once, at the entry.** Every outside value — request field, env var, file, network response, a row another writer controls — is validated or normalized where it enters, into a type that can only hold valid values. The code behind the entry takes that type and never checks it again.
2. **Every size has a named limit.** Lengths, counts, bytes, depth, retries, timeouts, and queue capacity each get a `const` with a domain name, enforced at the entry. Past the limit the entry rejects the input with an error that names the limit. Only content the system stores may be normalized instead: cutting a fetched page's title to its column length is fine, while turning a caller's requested depth of 100 into 25 is not. Nothing behind the entry grows without bound.
3. **Remove the special case, don't add one.** When an edge case needs its own branch, restructure so it takes the normal path — what Linus Torvalds calls good taste. A branch keyed on one specific value is a symptom patch; see [gate-bug-fix](../gate-bug-fix/SKILL.md).
4. **Fail at the entry, never crash the core.** Bad input returns an error at the boundary and never panics the process. `debug_assert!` states what the entry already guarantees.
5. **Cheapest check first.** Order work so validation and deduplication can stop it before network calls, model inference, or writes. Knowledge Base's content-hash check before enrichment is the current example.
6. **Smallest public surface.** `pub` only what another module calls, and keep a type's fields private when an invariant depends on them, so the checked constructor is the only way in. Callers depend on what a module does, never on the crate behind it — at a service boundary that becomes [gate-architecture](../gate-architecture/SKILL.md#contracts)'s contract rule.

## Durability (NASA Power of 10, Linux kernel style)

Sources: [NASA/JPL's Power of 10](https://en.wikipedia.org/wiki/The_Power_of_10:_Rules_for_Developing_Safety-Critical_Code) and the [Linux kernel coding style](https://www.kernel.org/doc/html/latest/process/coding-style.html). Both were written for C; what follows is the Rust adaptation, and where it departs from the source it says so.

From the kernel: more than 3 levels of indentation means the function needs restructuring; a function does one thing and fits on one or two screens, with no more than 5–10 locals; no new `BUG()` — the kernel warns with `WARN_ON_ONCE()` and keeps running. The kernel *wants* a comment at the head of a function saying what it does; this repo's no-comments rule above is stricter than the kernel and is our own.

From NASA: fixed loop bounds, no function past ~60 lines, every return value checked and every parameter validated, data at the smallest scope, zero warnings from day one. Three NASA rules are adapted rather than copied: NASA bans recursion outright, we allow it only when depth is provably bounded (a parser over a tree of known depth); NASA forbids heap allocation after startup, we bound it instead; NASA asks for two assertions per function on average, we assert real invariants and let types carry the rest.

**Control flow:** max 3 nesting levels, early returns over else-chains, every loop bounded, no recursion unless depth is provably bounded.

**Errors:** every `Result` handled — no `let _ = ...`. No `unwrap()`/`expect()` on input, I/O, or network. Errors carry context via `anyhow`/`thiserror`.

**Resources:** pre-allocate when size is known, bounded channels and queues, borrow instead of clone on hot paths, variables scoped to the smallest block, no leaked handles.

**Correctness:** `assert!`/`debug_assert!` for real invariants, no `unsafe` unless isolated and justified, Clippy clean with warnings denied in CI, types encode constraints (enums over strings, newtypes over raw primitives).

**Simplicity:** one responsibility per function; add a trait or layer only when a second implementation exists; don't add a crate for a five-line operation.

## Naming

| Avoid | Use |
|---|---|
| `data`, `info`, `temp`, `result` | `page_content`, `crawl_timestamp`, `embedding_vector` |
| `process()`, `handle()`, `do_work()` | `extract_main_content()`, `store_page_embedding()` |
| `get()` | `fetch_page_by_url()` |
| `"product"` string literal | `PageTypeEnum::Product` |

## Critical Findings

- Any comment past line 1
- `process`/`handle`/`util`/`helper`/`manager` without a domain prefix
- Unhandled error path, or `unwrap()` on external input
- Outside value used without an entry check or an upper limit, or a caller's out-of-range request silently changed instead of rejected
- Abstraction with one implementation and no second use planned
- File over 200 lines mixing responsibilities

## Example

```rust
// Extracts readable article text from crawled HTML.

fn extract_main_content(raw_html: &str) -> Result<String, ExtractionError> {
    let denoised = strip_navigation_and_ads(raw_html)?;
    Ok(denoised)
}
```

Versus: `fn process(data: &str)` with `// clean up noise` above a `clean()` call — unclear name, redundant comment, vague error type.
