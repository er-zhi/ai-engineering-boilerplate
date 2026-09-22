---
name: golden
description: Use when running or judging the golden cases — the end-to-end checks in golden/cases.json that exercise chat, the declarative tools and the knowledge base against a live stack.
---

# Golden

The golden cases are the product's own behaviour, checked end to end through Gateway — the same
door a person's browser uses. They are not unit tests and they are not in the workspace suite: they
need the stack up, real API keys, and a judgement no assertion can make.

**The runner decides nothing.** Answers are generated text; comparing them to stored strings
measures phrasing, and phrasing changes for free. So `cargo run -p golden` records what happened —
how many topics a message opened, what each was titled and asked, what came back, when each
finished — and you judge the run against each case's written expectation.

## Running

```bash
export PATH="$HOME/.cargo/bin:$PATH"
set -a && source .env && set +a
cargo run -p golden > /tmp/golden-run.json
```

One case only: `cargo run -p golden -- three-themes-with-a-reference`.
A different stack: `GOLDEN_BASE_URL=http://host:port`.

**Check the decision provider first.** When TypeSafe AI is overloaded every typed decision times
out at 2s, and the system falls back to a loop where a light model answers with a promise. Every
case then fails for one external reason and the run says nothing about the code:

```bash
curl -m 30 -o /dev/null -w '%{http_code} %{time_total}s\n' -X POST https://api.typesafe.ai/v1/systemone \
  -H "Authorization: Bearer $TYPESAFE_AI_API_KEY" -H 'Content-Type: application/json' \
  -d '{"model":"jev-1.13.0","state":"x","questions":{"q":{"type":"noul","instructions":"Does this need current information?"}}}'
```

`200` in well under a second means healthy. `529`, or a slow `200`, means stop — a run now measures
the provider. `/v1/models` answering fast is not evidence; it stayed fast through the last outage
while every decision timed out.

## Judging

Read the run. Each case carries its own `expect`, written as a sentence, beside what actually
happened. For every case decide PASS or FAIL against that sentence alone, and report:

| Case | Verdict | What happened |
|---|---|---|

Rules for judging, because these are the mistakes worth avoiding:

- **Judge behaviour, not wording.** "24.11" and "24.11 degrees Celsius, clear skies" both answer a
  weather case. A different phrasing is never a failure by itself.
- **A promise is a failure.** "I'll look that up", "I need to make separate calls", "Please
  confirm" — nothing runs after a reply, so the undertaking is never kept and the turn ended having
  done nothing. This is the failure the whole design exists to remove.
- **Read `question`, not just `title`.** A topic is executed with its own `question` and nothing
  else. One still saying "there" or "it" is a failure even when the answer happens to be right,
  because it was right by luck.
- **Check `finished_at_ms` for independence.** Themes are meant to arrive as each becomes ready. If
  three topics all finish within a few milliseconds of the last one, they were serialised.
- **A knowledge-base case fails on invention.** A named course, document or claim that the
  knowledge base does not hold is worse than "I could not find it". Search the knowledge base
  directly to check a doubtful one — the `kb` cases in the same run show what it actually returns.
- **An empty `topics` list is a real outcome**, not an error: a turn carrying no request opens
  nothing. `not-a-request` expects exactly that.
- **`error` set means the case could not be run.** Report it as a failure of the run, not of the
  behaviour, and say which.

Report every case, passes included — a table where only failures appear hides the run's size.
Do not fix anything while judging. Collect the verdicts, then decide what is worth changing.

## Adding a case

Add an object to `golden/cases.json`: `id`, `kind` (`chat` or `kb`), the `message` or `query`, and
`expect` as a sentence a person could check by hand. Write what must be true, not what the answer
should say — an expectation that quotes an exact answer will be wrong the next time the model is
changed. Say what a failure would look like where it is not obvious.
