# Capability Tree — Design

**Status:** rulings settled 2026-09-20, after reading the decision vendor's documentation in full
and putting the first draft of this design through an adversarial review against it.

**Problem.** What the application can do should be something we declare, not something a generative
model improvises each turn. The generative call is the expensive part of a turn — measured here at
p50 ≈ 2.0 s against a typed decision's p50 ≈ 156 ms — so every capability that a typed decision can
route and dispatch is a capability that answers roughly 1.8 s faster and does the same thing twice
in a row.

**Owner's constraint, given explicitly:** flexibility may be traded away, but not into regular
expressions and not into a pile of conditionals. Where a judgement genuinely needs a generative
model, we call one.

---

## What the vendor's documentation settles

Every claim here is quoted from a page that was read in full. Where the documentation is silent,
this spec says so rather than inferring, because an earlier round of this work shipped a fabricated
vendor citation into four files.

**There are exactly three primitives.** `noul`, `choice`, `score`. No extraction, slot filling,
entity or typed-value output. `primitives.md`: "There are three question types, each returning a
different shape of answer", and "Every answer is constrained to the options you supplied. The model
returns a probability distribution over your options or levels, never a value outside them." The
model card is blunter — `model-jaggedness/jev-1.13.md`: "`jev-1.13` is not trained to generate
text. While you can force it to by chaining choices, this will not work well and will be very
slow."

**Arguments come back with the chosen function, when they are closed.** `cookbooks/function_calling.md`
sends one request carrying the choice of function and every function's arguments, and the dispatcher
reads only the chosen function's answers. A `Literal` argument becomes a `choice` over exactly its
values, a `list[Literal]` becomes one `noul` per member, and an optional argument gains a second `stated`
noul asking whether the request says anything about it at all. A `bool` argument is sorted as its
own kind — "a **flag** (a `bool`, so on or off)" — but the cookbook never says which question type a
flag becomes; that it is a `noul` is our inference from the shape, not something the page states. Open-ended arguments are not filled: "Free text, numbers and dates work the same way: no
question, and the function's default stands."

**Ask everything in one round.** `patterns/fan-out.md`: "we recommend putting all of the questions
your system needs in a single request, and then using code to decide what is relevant after the
fact. All questions are evaluated in parallel, so adding more questions usually has little effect
on response time." A second round is legitimate only when "your code cannot build the second
request until it has the first answer" (`primitives.md`) — and it calls two requests "the
exception, not the rule".

**Confidence does not mean correctness.** `semantic_find` — "Choice probabilities always add up to
1, so some line ranks first even when the document doesn't answer the question." `concepts/system-one.md`
— "Calibration is measured across groups of predictions; it does not guarantee that an individual
answer is correct." A two-option choice where neither option is right can still be peaked and
confident, because confidence measures separation between the options offered, not whether any of
them fits.

**Published figures.** `concepts/how-to-build-with-system-one.md`: "Most queries complete in about
100 ms." `models.md` carries the token and rate ceilings — 64k per request, 32k for state plus the
longest question, 250 000 tokens/second and 1 200 requests/minute — and the finding that costs us
nothing to act on: "If you have tuned confidence thresholds against a specific version, pin that
version's ID instead of the alias." The per-question ceilings are on `api.md` instead: "a maximum of
255 options per Choice", and "A Score should have at least two levels; the API accepts up to 10."

**No question-count limit is documented anywhere.** Our `MAX_QUESTIONS = 64` is ours. The vendor's
own function-calling example sends 54 questions for ten functions.

**The documentation argues for this design in general terms.** `concepts/how-to-build-with-system-one.md`:
"System One is TypeSafe's model for building AI-powered software, not agents. It does not generate
code or choose its own next action." And: "Avoid agent `while` loops when a software workflow can
express the same behavior."

---

## Rulings

### R1. The tree is worth building only for closed, multi-argument, non-numeric capabilities

This is the ruling that survived review, and it contradicts the first draft.

Count generative calls, because only they cost ~1.8 s:

| shape | today | under the tree |
|---|---|---|
| tool with one required string field | 1 (the answer) | 1 — **no change** |
| tool with ≥2 arguments, all closed | 2 (compose arguments, then answer) | **1** |
| tool with any open-ended argument | 2 | 2 — **no change**, and worse if we add a round |

The fast path in `services/engine/src/executors/llm.rs` already dispatches a single-string tool with
no generative call, passing the question verbatim. The tree adds nothing there. It pays exactly
where today's `llm` node has to compose arguments generatively.

**Therefore a capability whose arguments are not all closed does not enter the tree.** It stays on
the existing paths. Adding a generative call to fill an argument and then another to phrase the
answer is strictly worse than what we run today, and the vendor's own cookbook never fills an open
argument — it drops it.

### R2. No regular expressions

The vendor documents a `pre_parsed_value_extraction` recipe — a regex over-finds candidate spans,
the model picks one, code copies it verbatim. It is rejected here by the owner's constraint. The
consequence is accepted and stated in R1: open-ended values cost a generative call, so capabilities
that need one stay off the tree.

### R3. The answer to the user is always generative

Rendering the answer from a template in the operator's file would put the conditionals back, one
branch per capability, in the renderer instead of the router. One generative call per turn is the
floor, and it is the answer. We do not try to go below it.

### R4. Every closed set carries an escape, and every capability carries an absolute gate

Omitted from the first draft; review caught it. Two mechanisms, both from the vendor:

- an escape option on every closed set — `primitives.md`: "add an `other` or `none of the above`
  option when the list might not cover every input";
- an absolute `noul` beside the relative `choice` — the model card: "A Choice over options and one
  Noul per option answer different questions: the Choice is relative, settling *which* option, while
  each Noul is absolute and can be low for all of them."

Without both, an operator declares a list closed, a request arrives for something not in it, and the
tree dispatches the nearest match with high confidence. That is the design's worst failure mode: not
a lost turn, but a fluent generative answer composed over the wrong tool's output.

### R5. Thresholds are data, and they are applied in the deciding node

`engine_core::Condition` compares with `Eq`, `Truthy` and `Exists` — it has no numeric comparison. A
`noul` is a float and a `choice` carries a confidence. So either the deciding node applies its
thresholds and writes **booleans and the chosen label** into state, or every threshold lands in Rust
as a per-capability conditional.

**Ruling:** the node applies them. Thresholds live in its config, which is graph data. `Condition`
is not extended. Nothing about a capability reaches Rust.

Two thresholds are needed, not one: the capability choice, and the weakest argument.
`cookbooks/function_calling.md`: "`confidence` reports the least certain judgement in the call…
one wrong argument is enough to spoil the result." And they are not interchangeable with noul
probabilities — that warning is the model card's, `model-jaggedness/jev-1.13.md`: "Don't carry a
threshold tuned on a Noul over to a Choice."

### R6. Below threshold, fall to the generative node — a decision must never lose a turn

Unchanged from every previous round of this work, and it is also the vendor's `intent-routing`
shape: an uncertain intent routes to the expensive handler, a confident one to deterministic code.

### R7. The invariant: adding a capability is a file edit, never a code edit

This is what "Capabilities, Not Topics" means when stated as a design rule instead of a prohibition,
and it is the thing worth testing. If adding a weather capability requires touching Rust, the design
has failed regardless of how fast it answers.

### R8. One speculative round

Reversed from the first draft, which proposed two. The argument questions are static data from the
operator's file; code can build every question before the turn begins, so the vendor's stated
exception ("it needs the answer… to pick the next question's options") does not apply. Our own
64-question ceiling was the only reason for a second round, and it is ours to raise. What binds
first is tokens, not latency — `primitives.md`: "Adding questions barely changes the response time
and costs only the tokens for the extra questions."

A second round is reserved for the one case the vendor does show — re-ranking the top few
capabilities against richer descriptions than the first round could afford. We do not build it now.

---

## Open questions, held deliberately

**Does any capability qualify?** Under R1 the tree pays only for closed, multi-argument, non-numeric
tools, and **no declarative row exists yet** — the gain set is empty today. The first task that
matters is therefore not building the tree; it is establishing, with a real row against real
endpoints, whether such a capability exists in practice. If it does not, this design does not get
built, and that outcome is a legitimate result.

**Does a free-text source tolerate the whole question as its argument?** Review's fallback for
open-argument capabilities is the existing single-string fast path, which passes the user's whole
message as the argument. That is only sound if the endpoint does fuzzy matching. It is one request
to find out, and the answer decides whether such capabilities have any typed path at all.
