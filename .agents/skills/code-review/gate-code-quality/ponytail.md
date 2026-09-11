# Ponytail Pass

Deletion-only review, adapted from [Ponytail](https://github.com/DietrichGebert/ponytail). Hunts complexity, nothing else — correctness, security, and performance belong to other gates. Lists findings; applies nothing.

The best outcome is a shorter diff.

## Format

One line per finding: `path:Ln-Lm: tag: what to cut → replacement`

| Tag | Meaning |
|---|---|
| `delete:` | Dead code, unused flexibility, speculative feature. Nothing replaces it. |
| `stdlib:` | Hand-rolled thing std ships. Name the function. |
| `native:` | Dependency doing what the platform already does. Name the feature. |
| `yagni:` | One-implementation abstraction, config nobody sets, layer with one caller. |
| `shrink:` | Same logic, fewer lines. Show the shorter form. |

## Hunt

Single-implementation traits, factories with one product, wrappers that only delegate, files exporting one trivial thing, dead flags and config, dependencies std already covers, hand-rolled `Iterator`/`Option` combinators.

## Examples

```
src/repo.rs:L88: yagni: PageRepositoryTrait with one impl. Inline until a second store exists.
src/util.rs:L52-71: delete: retry wrapper around an idempotent local call. Nothing replaces it.
src/map.rs:L30-44: shrink: manual loop builds HashMap. keys.into_iter().zip(values).collect(), 1 line.
Cargo.toml:L14: native: chrono for one format call. std formatting is enough.
```

End with `net: -N lines possible.` Nothing to cut: `Lean already. Ship.`

A single smoke test or `assert!` is the minimum safety bar — never flag it for deletion.
