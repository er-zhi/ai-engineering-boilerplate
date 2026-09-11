---
name: gate-code-quality
description: Use when writing or reviewing Rust in this boilerplate, or when the user mentions NASA rules, comments, naming, file size, over-engineering, or what can be deleted.
---

# Code Quality

Durable code is simple, explicit, and self-explaining. Names replace comments; small files replace abstraction layers.

Report findings using the template in [code-review](../SKILL.md). For deletion-only passes, use [ponytail.md](ponytail.md).

## Style Rules

1. **No comments** — the only exception is one summary line at line 1 of a file: `// Fetches and stores page embeddings.` States what the file does, not how.
2. **Self-documenting names** — a reader needs no explanation beyond the identifier.
3. **Minimal files** — under ~200 lines; split only when a file does two distinct jobs. Functions under ~60 lines.
4. **No dead weight** — no unused imports, no speculative abstractions, no wrapper that only delegates.

## NASA Durability (Rust)

**Control flow:** max 3 nesting levels, early returns over else-chains, every loop bounded, no recursion unless depth is provably bounded.

**Errors:** every `Result` handled — no `let _ = ...`. No `unwrap()`/`expect()` on input, I/O, or network. Errors carry context via `anyhow`/`thiserror`. Input validated at boundaries.

**Resources:** pre-allocate when size is known, no unbounded growth in hot paths, variables scoped to the smallest block, no leaked handles.

**Correctness:** `assert!`/`debug_assert!` for real invariants, no `unsafe` unless isolated and justified, Clippy clean, types encode constraints (enums over strings, newtypes over raw primitives).

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
