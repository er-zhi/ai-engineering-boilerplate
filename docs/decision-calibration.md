# Decision calibration

Every threshold the typed decisions are read against, the measurement that set it, and what to
re-run when the model version changes.

All numbers below were taken against **`jev-1.13.0`**, by hand, over the question sets given. A
threshold is only as good as the version it was measured on: `.env.example` pins an exact version
rather than an alias for this reason. On a version bump, replay each table and move the threshold to
the middle of the new gap — or, if the gap closes, stop relying on that decision rather than tune it
tighter.

## Scope: which source answers a question

`TOOL_CHOICE_CONFIDENT_ENOUGH_TO_ACT_ON = 0.5` — `services/engine/src/executors/llm.rs`

One choice over the live catalog plus an option meaning none of it covers the question. 18 questions,
against the catalog this deployment runs:

| Band | Score | Questions |
|---|---|---|
| Reached its source | 0.65 – 1.00 | 9 knowledge-base questions, weather, an FX pair, an index |
| Reached `none` | 0.99 – 1.00 | a capital city, arithmetic, a football result, "write me a script", a subject the store does not hold, a greeting |

18 of 18. The gap is wide enough that the exact threshold decides nothing, which is the point of
asking the catalog rather than the model.

**What this measures is the catalog's descriptions, not the model.** A description that does not say
what a source covers scores `none` for everything it holds: the knowledge-base tool, described only
as "searches this project's knowledge base", reached `none` for every question its own corpus
answered. When a source is added or its contents change, re-run this table before trusting it.

## Argument: does the message point at the value

`POINTED_AT_ENOUGH_TO_DISPATCH = 0.8` — `services/engine/src/executors/llm.rs`

Asked of a composed argument before anything is dispatched. 18 pairs of (message, composed value):

| Band | Score | Pairs |
|---|---|---|
| Refuse | 0.31 – 0.70 | four forms of "there", "it", "the other one", a place invented outright, a place appearing where the message named none |
| Dispatch | 0.82 – 0.97 | a place named; a place described ("the capital of Japan"); currency codes normalised from "dollar to euro"; an exchange symbol normalised from "NASDAQ"; a pair normalised from "bitcoin in dollars"; a place named in Russian |

18 of 18, gap 0.70 → 0.82.

**The phrasing is load-bearing and two others were rejected on measurement.** "Does the message name
this value in so many words" scores 0.05 for a value the message describes exactly, so it refuses
every inference and normalisation. "Could it have been anything else" puts an exchange symbol at
0.59 against a pronoun at 0.61 — too fine to threshold. The shipped phrasing asks whether the
message *points at* the value, which a name, an unambiguous description and a normalisation of
either all do.

## Reply: is it grounded in what came back

`GROUNDED_ENOUGH_TO_SEND = 0.5` — `services/engine/src/executors/llm.rs`

Asked of (material, reply) after the answer is written:

| Reply | Score | |
|---|---|---|
| A place name, when the material holds a temperature | 0.05 | caught |
| A figure the material does not contain | 0.03 | caught |
| The value, with the place | 0.95 | sent |
| A bare value | 0.98 | sent |
| Passages quoted back | 0.82 | sent |

**A guard measured and rejected.** "Does the reply answer the question?" scores a bare `22.11` at
0.26 and an invented `31.4 degrees` at 0.81 — it refuses correct answers and passes invented ones.
For a system whose house style is a bare value, that guard is inverted. Do not reach for it again
without re-measuring.

**What this guard cannot see.** A reply faithful to material fetched for the wrong question passes:
the pronoun turn, where `place: "there"` reached a village of that name, scored 0.96. That is why the
argument guard above exists upstream of it.

## Reply: does it only undertake work

`PROMISE_ENOUGH_TO_REFUSE = 0.5` — `services/engine/src/executors/llm.rs`

A reply that says what it is about to do, rather than reporting what came back, ends the turn having
done nothing. Measured against the live failure ("Bishkek. I need to look up the weather there and
the NASDAQ index") and its correct counterparts.

**Stating the fault is not enough.** Told only that it was wrong, the light model writes the same
reply twice in a row — measured. The correction quotes the refused words back, and with the quote
the same three questions answer from their material.

## Message: does it stand on its own

`REWRITE_WHEN_STANDS_ALONE_BELOW = 0.5` — `services/chat/src/intent.rs`

Asked of the message on the routing decision. 12 messages:

| Band | Score | Messages |
|---|---|---|
| Leans on the conversation | 0.05 – 0.10 | "weather there?", "where?", "and in Osaka?", "how about it?", "and tomorrow?" |
| Stands on its own | 0.82 – 0.98 | a weather question naming its place, a capital question, "NASDAQ today", a knowledge-base question |

11 of 12 at 0.5. The twelfth is "thanks, that's helpful" at 0.09, which is correct — it does not
stand alone as a request — and is settled earlier by the `actionable` question.

## Material: can an earlier turn's records answer this

The `already_found` option's description, `services/engine/src/executors/llm.rs`

Three wordings measured over six follow-ups. The shipped one routes five correctly: a question about
a value inside the material, a question about what was looked up, and a rephrasing all reach the
material, while "and what about Osaka?" still fetches afresh and an out-of-scope question still
declines.

A bare "where?" reaches `none` at 0.23–0.51 under every wording — the question is too empty to route
on. It is answered because Chat rewrites it against the session first.

## Latency, for the cost argument

Measured on this deployment, same day:

| Call | p50 |
|---|---|
| `Decide` | ~150 ms |
| `Complete` | ~2000 ms |

Every generative call a decision removes is worth about 1.8 s. Carrying an earlier turn's material
into a decision's state costs almost nothing: 229 ms at nothing carried, 289 ms at 48 000
characters.

## Incidents these numbers came from

Kept because each explains why a guard exists, and a threshold without its failure reads as
arbitrary.

- A question whose answer the connected store held was answered from the model's own memory, and
  answered wrongly, because the decision asked whether the model *needed* to look something up — a
  question about the model's confidence, not about the catalog.
- Asked for the weather in a capital named indirectly, the model replied with the city's name. The
  source had run and returned a temperature; the reply reached past it.
- "weather there?" composed the argument `place: "there"`. That is a real village in Pakistan. Its
  weather was returned as the answer to a question about Japan, and every guard passed it.
- A 429 arrived as HTTP 200 with `finish_reason: "error"` and half a JSON object as content. The
  fragment was shown to a person as the answer.
- The decision provider returned `529 system_overloaded` twice in one working day, once for about
  forty minutes, with a successful call taking 13.8 s against a normal 270 ms.
