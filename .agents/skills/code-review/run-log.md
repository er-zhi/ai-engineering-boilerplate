# Run Log

One line per review, last 10 kept. Its only job is to show which gate is *chronically* slow, so optimization effort lands where it changes the wall clock.

Path: `.agents/skills/code-review/logs/runs.jsonl`

## Format

One JSON object per line. Record only the gates that actually ran — a skipped gate must not dilute its own average.

```json
{"ts":"2026-09-11T12:00:00Z","scope":"services/crawler","verdict":"request_changes","elapsed_s":34,"gates":{"architecture":4,"code-quality":5,"testing":34,"facts":12},"critical":2,"warnings":3}
```

`elapsed_s` is wall clock for the whole review, which under real parallelism equals the slowest gate.

## Write and Rotate

Append, then truncate to the last 10. One command, no script to maintain:

```bash
mkdir -p .agents/skills/code-review/logs && L=.agents/skills/code-review/logs/runs.jsonl && \
  echo '<json line>' >> "$L" && tail -n 10 "$L" > "$L.tmp" && mv "$L.tmp" "$L"
```

## Find the Slow Gate

```bash
jq -s '
  [.[].gates | to_entries[]]
  | group_by(.key)
  | map({gate: .[0].key, runs: length, avg_s: ((map(.value) | add) / length | .*10|round/10), max_s: (map(.value) | max)})
  | sort_by(-.avg_s)
' .agents/skills/code-review/logs/runs.jsonl
```

```json
[
  { "gate": "testing",        "runs": 2, "avg_s": 29.5, "max_s": 38 },
  { "gate": "second-opinion", "runs": 1, "avg_s": 22,   "max_s": 22 },
  { "gate": "facts",          "runs": 3, "avg_s": 11,   "max_s": 13 }
]
```

## Confirm Gates Ran in Parallel

A review whose elapsed time approaches the sum of its gates was not parallel, and no gate-level tuning will fix that.

```bash
jq -r '([.gates[]] | add) as $sum |
  "\(.ts)  elapsed=\(.elapsed_s)s  sum=\($sum)s  \(if .elapsed_s >= $sum * 0.9 then "SERIAL" else "parallel ok" end)"
' .agents/skills/code-review/logs/runs.jsonl
```

## Acting on It

Four rules, in order of how often they save wasted effort:

1. **Only the slowest gate matters.** Gates run in parallel, so total is the slowest one. Speeding up any other gate changes the wall clock by zero.
2. **Wait for `runs` ≥ 3.** A gate seen once has no average worth trusting. The `runs` field is there to stop you optimizing a single sample.
3. **`max_s` far above `avg_s` is a spike, not a problem** — a cold cache or a slow network call. Chronic slowness is `max_s` close to `avg_s`. Fix chronic, ignore spikes.
4. **Fix the cause, never drop the gate.** A gate that runs tests or calls another model is *supposed* to cost tens of seconds.

| Chronically slow | Usual cause | Where to look |
|---|---|---|
| `testing` | Slow suite, not a slow gate | [gate-testing](gate-testing/SKILL.md) timing report |
| `second-opinion` | Two model calls, or run serially | Confirm both CLIs launch in parallel |
| `facts` | Many lookups, or web search over Context7 | Batch queries; Context7 before web search |
| `evidence` | Full stack boot to prove one thing | Narrow the command |
| Everything, evenly | Gates read more context than they need | One gate per agent, diff only |
