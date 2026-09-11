---
name: gate-testing
description: Use when reviewing a diff that touches Rust code, or when the user asks to run the tests, check test coverage of a change, find slow tests, or speed up the test suite.
---

# Testing

Run the suite. Report what failed and what was slow. A suite nobody waits for is a suite nobody runs, so timing is part of the verdict here — not a nicety.

Report findings using the template in [code-review](../SKILL.md), and include the timing block below.

Not this gate's scope: ad-hoc proof that a change works by hand belongs to [gate-evidence](../gate-evidence/SKILL.md). This gate owns the suite itself — whether tests exist, pass, and stay fast.

## Running

Run the suite the way [cargo-nextest](https://nexte.st/) documents it. It prints each test's wall-clock time in brackets and marks anything over 60s `SLOW`, which is exactly the data this gate reports.

```bash
cargo nextest run --workspace           # whole suite
cargo nextest run -p crawler            # one service
cargo nextest run --status-level slow   # surface slow tests during the run
```

Storage tests start their own Postgres through testcontainers, so Docker must be running. Never wrap these commands in a script — see [gate-architecture](../gate-architecture/SKILL.md#docker--compose).

Do not reach for `cargo test -- --report-time`. That flag is nightly-only and needs `-Zunstable-options`; on stable it fails with `the "report-time" flag is only accepted on the nightly compiler`. Plain `cargo test` is the fallback when nextest is unavailable, and it gives no per-test timing.

## Timing Report

Always report these four numbers, then the slowest tests. This is what makes the next optimization obvious.

```
Suite: 47 tests, 41 passed, 0 failed, 6 skipped
Wall clock: 18.4s  (budget 120s)
Slowest:
  12.1s  crawler::tests::crawl_respects_exclude_patterns   ← 66% of total
   2.3s  gateway::tests::search_proxies_to_crawler
   0.9s  crawler::denoise::strips_nav_and_footer
```

One test owning most of the runtime is the finding. Name it and say why it is slow.

## Common Causes of Slow

| Symptom | Usual cause | Fix |
|---|---|---|
| One test dominates | `thread::sleep` waiting for readiness | Poll for the condition instead |
| Every integration test slow | A testcontainer per test | One container per suite, transaction rollback per test |
| Suite slow but each test fast | Debug-build compile time | Cache `target/` in a volume; it is build, not test |
| Unit test takes seconds | Real network or real DB | Not a unit test — move it or fake the boundary |
| Suite slower than the sum of its tests | Shared mutable DB state forcing serial runs | Isolate per-test data so it runs parallel |

## Layers

Meaningful, fast, reliable — not coverage theater.

| Layer | Covers |
|---|---|
| Unit | Domain logic, mappers, pure functions — nothing outside the process |
| Integration | Storage against a real Postgres through testcontainers; HTTP and gRPC against servers the test starts on `127.0.0.1`, never the internet |
| Contract | Proto compatibility between services |

## Quality Checks

- Diff changes behavior but adds no test → Critical
- Test asserts a mock was called rather than a real outcome → Warning
- Unit test touches network or DB → Warning, reclassify as integration
- Tests depend on execution order or shared state → Critical, they will flake
- Suite over the 2-minute budget → Warning with the slowest test named
- Test name does not state what it proves → Suggestion

## Severity

| Severity | Condition |
|---|---|
| Critical | Any test fails, or behavior change ships untested, or a test is order-dependent |
| Warning | Suite over budget, unit test crossing a boundary, assertion proves nothing |
| Suggestion | Naming, structure, a faster way to assert the same thing |

If the suite cannot run at all, say why in one line and stop. A guess about whether tests pass is worth nothing.
