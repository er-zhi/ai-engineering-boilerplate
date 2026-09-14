# Embedder (ANE)

Turns text into a vector on the Mac's Neural Engine — the dedicated ML accelerator every Apple Silicon chip carries beside its CPU and GPU. This is the one process in the stack that is not a Docker service: `knowledge-base` calls it over plain HTTP at `EMBEDDER_URL` — `GET /health`, `POST /embed/document`, `POST /embed/query`.

## Why Outside Docker

Core ML, the only route to the Neural Engine, is a macOS framework; a Linux container can never reach it. So this runs as a plain process on the host, and `knowledge-base`'s container reaches it over `host.docker.internal`. See [gate-architecture](../../.agents/skills/code-review/gate-architecture/SKILL.md#boundaries) for the rule this is the sanctioned exception to.

## Model

[`google/embeddinggemma-300m`](https://huggingface.co/google/embeddinggemma-300m) — 768-dimensional output, Apache 2.0 (Gemma license), a bidirectional encoder adapted from the decoder-only Gemma 3 line, using mean pooling over token embeddings followed by two linear projections (768→3072→768). `google/embeddinggemma-300m` is gated on Hugging Face; [`unsloth/embeddinggemma-300m`](https://huggingface.co/unsloth/embeddinggemma-300m) is the same published weights, ungated, used by `convert.py` so a fresh machine never needs to request the gate.

Replaced `Qwen/Qwen3-Embedding-0.6B` (archived at `../embedder-ane-qwen3-archived`, restorable by moving it back if this ever needs reverting). Qwen3 scores slightly higher on raw MTEB, but at roughly 5x the latency for a page-search corpus where 9ms vs. 20-100ms end-to-end is the more visible difference.

### Why a hand-written Core ML conversion, not a published one

`coremltools.convert()` on the model's own attention module (`Gemma3Attention`, which uses PyTorch's fused `scaled_dot_product_attention`) fails outright — the Apple Neural Engine compiler cannot compile the fused op (`ANECCompiler: ANECCompile() FAILED`), and Core ML silently falls back to CPU-only, undetectably, unless checked with `MLComputePlan` (see below). `convert.py` instead reimplements attention as explicit `matmul` → `+mask` → `softmax` → `matmul`, matching the model's published math exactly (verified by cosine similarity against `SentenceTransformer.encode`, see below) but expressed in ops the ANE compiler accepts.

Third-party pre-converted `.mlpackage` files exist (e.g. `mlboydaisuke/embeddinggemma-300m-coreml`) and work, but running someone else's compiled binary in production — rather than a script this repo can read, re-run, and re-verify — was judged not worth the time saved. The hand-written version also turned out ~40% faster (5.8 ms vs. their measured 9.4 ms on the same M4, both at 8-bit, both >98% Neural-Engine-resident per `MLComputePlan`) — the published build's fixed shape and residual layer-norm placement differ slightly from what `convert.py` produces.

### Architecture specifics that mattered for the conversion

- **Bidirectional, not causal** (`use_bidirectional_attention: true` in the model config) — no autoregressive mask, unlike most Gemma 3 checkpoints.
- **Grouped-query attention**: 3 query heads share 1 key/value head (`num_key_value_heads: 1`), every head — query and KV alike — is 256-wide. That's set directly by `head_dim` in config, not derived by dividing hidden size by head count: it happens to match for the 3 query heads (768/3 = 256), but the single KV head would need a 768-wide head to match hidden size the same way, and it's still 256 — head width is configured independently of hidden size in Gemma 3.
- **Alternating attention window**: layers 1-5, 7-11, 13-17, 19-23 (5 of every 6) are sliding-window (radius 257, `rope_local_base_freq: 10000.0`); layers 6, 12, 18, 24 are full attention (`rope_theta: 1000000.0`) — two different RoPE frequencies depending on layer type. At `SEQ_LEN = 128` the sliding window never actually restricts anything (max distance 127 < 257), but `convert.py` builds the real windowed mask rather than relying on that coincidence, so it stays correct if `SEQ_LEN` ever changes.
- **QK-norm**: `q_norm`/`k_norm` (RMSNorm) are applied per-head, after the head split, before RoPE — order matters.
- **Sandwich normalization**: each block is `input_layernorm → attention → post_attention_layernorm → +residual`, then separately `pre_feedforward_layernorm → MLP → post_feedforward_layernorm → +residual` — four RMSNorms per layer, not the usual two.
- **Embedding scale**: token embeddings are multiplied by `sqrt(hidden_size)` (≈27.71) — already applied inside the HF `Gemma3TextScaledWordEmbedding` module `convert.py` reuses directly, so don't apply it a second time (an earlier draft did, and produced embeddings with cosine similarity 0.18 against the reference — a nonsensical result that was the first sign of the bug, not a subtle one).
- **Two extra Dense layers** after pooling (768→3072, then 3072→768, no bias, no activation between them) are part of the published model, not an add-on — `sentence_transformers` calls them `2_Dense`/`3_Dense`; `convert.py` takes their weights (`st_model[2].linear`, `st_model[3].linear`) directly.

None of this is unique to this model — it's Gemma 3's decoder architecture adapted into a bidirectional encoder. The same technique (rewrite fused attention as explicit ops, verify by cosine similarity, then convert) fixed the identical `ANECCompile()` failure on a plain BERT model (`DistilBERT`) during evaluation; see the design spec history for that comparison.

## Measured on an M4

| | Qwen3-Embedding-0.6B (previous) | EmbeddingGemma-300M (`convert.py`, 8-bit) |
|---|---|---|
| `CPU_AND_NE`, direct Core ML call | ~19 ms (query) / ~20-100 ms (document, depends on length) | **5.8 ms** |
| Through this HTTP server, connection reused (the real caller's behavior) | ~20 ms | **~6.9 ms** |
| Neural Engine residency (`MLComputePlan`) | not separately measured | 2001/2025 ops (98.8%) |
| Cosine similarity vs. unquantized `SentenceTransformer.encode` | — | 0.996-0.997 (8-bit); 1.000 (fp16, unquantized manual rewrite vs. reference) |

`CPU_AND_GPU` (Metal) measured **~12.6 ms** and plain `CPU_ONLY` **~8.8 ms** on the same package — the Neural Engine is not merely available here, it is the fastest path by a wide margin for this workload shape (small batch, short fixed sequence). `ALL` (which lets Core ML pick GPU for some ops) measured faster than `CPU_AND_NE` on an *earlier, non-ANE-resident* build of this model — that was GPU doing the ANE compiler's failed work, not a real ANE result; once the model actually compiles for ANE, `CPU_AND_NE` wins outright and is the only mode this server uses, matching the project's policy of never routing through the shared GPU for an inference this small (see `gate-architecture`).

## Portability

Core ML runs on Apple Silicon only. If this stack ever moves to a Linux server, the embedder has to be re-implemented there — `convert.py`'s `ManualGemma3Encoder` is plain PyTorch with no Core ML dependency until the final `ct.convert()` call, so the same module could run on CUDA directly; only the last few lines of `convert.py` would need to change. Moving embedders (this one or a future one) always means re-embedding every document (truncate `knowledge_base.document_chunks`, re-crawl) — vectors from different models or even different quantization levels are not comparable, so partial migration is never safe.

## Setup

Python 3.12 (3.13 should also work — `coremltools` 9 ships wheels up to 3.13; 3.14 has no wheel and fails at import with a NumPy ABI error).

```bash
cd native/embedder-ane
python3.12 -m venv .venv
source .venv/bin/activate
pip install -r requirements.txt
```

Model files are not committed (see `.gitignore`) — `models/embeddinggemma-300m-8bit.mlpackage` (~295 MB) and `models/tokenizer.json` (~32 MB) are rebuilt with `convert.py`, not checked in.

### Rebuilding the model on a fresh machine

`convert.py` needs a separate, heavier set of dependencies than the server (`torch`, `transformers`, `sentence-transformers`) — install them into their own virtualenv rather than the server's `.venv`, since they're only needed once per machine:

```bash
cd native/embedder-ane
python3.12 -m venv .convert-venv
source .convert-venv/bin/activate
pip install -r requirements-convert.txt
python3 convert.py
```

This downloads `unsloth/embeddinggemma-300m` (~1.2 GB, ungated — no Hugging Face login needed), reimplements its attention as ANE-compilable ops, verifies the rewrite against the original with a cosine-similarity check (aborts if it drops below 0.999), traces and converts to Core ML, quantizes to 8-bit, and writes both `models/embeddinggemma-300m-8bit.mlpackage` and `models/tokenizer.json`. Takes a few minutes; most of it is downloading the source weights once.

To confirm a rebuilt package is actually running on the Neural Engine (not silently falling back to CPU — Core ML does this without an error, see above), check with `MLComputePlan` rather than trusting `compute_units` alone:

```swift
let plan = try await MLComputePlan.load(contentsOf: compiledModelURL, configuration: config)
// walk plan.modelStructure's operations, plan.deviceUsage(for:).preferred per op
```

A `.mlpackage` must be compiled to `.mlmodelc` first (`MLModel.compileModel(at:)`) before `MLComputePlan` can load it.

## Running

```bash
cd native/embedder-ane
source .venv/bin/activate
uvicorn server:app --host 0.0.0.0 --port 8086
```

Startup takes 30-60 s while Core ML compiles the graph for the Neural Engine; `uvicorn` prints nothing until that is done, then `Application startup complete`. `GET http://localhost:8086/health` confirms it is up.

## API

```bash
curl -s localhost:8086/health
curl -s -XPOST localhost:8086/embed/document -H 'content-type: application/json' -d '{"text":"Wireless noise-cancelling headphones."}'
curl -s -XPOST localhost:8086/embed/query -H 'content-type: application/json' -d '{"text":"best headphones for travel"}'
```

Response: `{"values": [...], "model_used": "google/embeddinggemma-300m", "truncated": false}`. Empty text is `400`; text over `MAX_REQUEST_TEXT_CHARS` is `413`; the fixed shape is 128 tokens — longer input is truncated and the response sets `"truncated": true`. Knowledge Base splits pages into passages sized to fit; a `truncated` response there means an unusually dense passage, not a bug.

`/embed/query` prepends `"task: search result | query: "`, `/embed/document` prepends `"title: none | text: "` — the model's own documented task prefixes for asymmetric retrieval. Getting this backwards quietly hurts ranking rather than erroring, which is why the two routes exist instead of a single one with a flag. Both prefixes and `SEQ_LEN` live in `config.py`, imported by both `server.py` and `convert.py` — the two must never define these independently, since a mismatch between the compiled `.mlpackage`'s fixed input shape and what the server actually sends would fail silently, the same way ANE fallback does.

One `threading.Lock` serializes `predict` calls: there is a single Neural Engine, and letting uvicorn's thread pool pile concurrent calls into Core ML only produces worse tail latency than queueing them here.

No request log — unlike `llm-router`'s `requests`/`request_payloads` tables — because a local, free, unmetered forward pass has nothing to audit.
