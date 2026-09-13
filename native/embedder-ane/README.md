# Embedder (ANE)

Turns text into a vector on the Mac's Neural Engine — the dedicated ML accelerator every Apple Silicon chip carries beside its CPU and GPU. This is the one process in the stack that is not a Docker service: `knowledge-base` calls it over plain HTTP at `EMBEDDER_URL` — `GET /health`, `POST /embed/document`, `POST /embed/query`.

## Why Outside Docker

Core ML, the only route to the Neural Engine, is a macOS framework; a Linux container can never reach it. So this runs as a plain process on the host, and `knowledge-base`'s container reaches it over `host.docker.internal`. See [gate-architecture](../../.agents/skills/code-review/gate-architecture/SKILL.md#boundaries) for the rule this is the sanctioned exception to.

## Model

[`Qwen/Qwen3-Embedding-0.6B`](https://huggingface.co/Qwen/Qwen3-Embedding-0.6B) — 1024-dimensional output, 32k-token context, Apache-2.0, a decoder-only model (last-token pooling, an instruction prefix on queries only) — converted to Core ML by [`neuradex/Qwen3-Embedding-0.6B-CoreML-ANE`](https://huggingface.co/neuradex/Qwen3-Embedding-0.6B-CoreML-ANE) and then weight-quantized to 8-bit here with `coremltools.optimize`. Chosen over the small BERT-family models the original design named because, with a real accelerator, model quality is the only axis left to optimize; the full history is in the [design spec](../../docs/superpowers/specs/2026-09-11-knowledge-base-design.md#embedding-model-native-apple-silicon-only).

Two fixed-shape compiled graphs, because the Neural Engine cannot run a dynamic-length graph: `b1_s128` (up to 128 tokens — every search query, most short documents) and `b1_s512` (up to 512 tokens). `server.py` picks the smaller one that fits. Text past 512 tokens is embedded from its first 512 and the response says `"truncated": true` — a head-only vector still finds the page, whereas refusing it would fail the caller's all-or-nothing ingest and, because the crawler never re-sends an unchanged page, drop that page from search for good. In practice knowledge-base splits pages into passages sized to fit before embedding, so truncation only happens for unusually dense text, and knowledge-base logs it.

One `threading.Lock` serializes `predict` calls: there is a single Neural Engine, and letting uvicorn's thread pool pile concurrent calls into Core ML only produces worse tail latency than queueing them here. A crawl's document embeds therefore queue ahead of a user's query embed; if that ever shows in search latency, the fix is a priority queue or a second process for queries.

`/embed/query` prepends the model's documented instruction prefix; `/embed/document` does not. The two routes exist so a caller cannot get this backwards — it would quietly hurt ranking rather than error.

## Measured on an M4

| Precision | Latency, `b1_s128` | Cosine similarity vs the unquantized model |
|---|---|---|
| fp16 (as published) | 24.6 ms | 1.000 |
| **8-bit (what runs here)** | **18.6 ms** | **0.999** |
| 6-bit | 19.0 ms | 0.993 |
| 4-bit | 19.0 ms | 0.50–0.60 — unusable |

8-bit is the whole win: faster than fp16 with no measurable quality loss. 6-bit is no faster and slightly worse; 4-bit palettization destroys this model's embeddings outright. Below 8-bit the cost is no longer in the weights, so further compression buys nothing. Through the HTTP server the steady-state call is ~20 ms.

The same model on the GPU via `candle`'s Metal backend measured ~190 ms on this machine, and PyTorch's MPS backend the same — at batch size one the model is memory-bandwidth-bound on the GPU, so the Neural Engine's ~10x is not something a different GPU framework could have matched. Search as a whole measures ~26–45 ms through Gateway — see [knowledge-base's README](../../services/knowledge-base/README.md#search).

## Portability

Core ML runs on Apple Silicon only. If this stack ever moves to a Linux server, the embedder has to be re-implemented there (the same weights run on CUDA via `candle` or PyTorch) — and because a different runtime and precision do not produce bit-identical vectors, that move also means re-embedding every document (truncate `knowledge_base.documents`, re-crawl) rather than mixing stored vectors from one implementation with queries from another.

## Setup

Python 3.12 (3.13 should also work — `coremltools` 9 ships wheels up to 3.13; 3.14 has no wheel and fails at import with a NumPy ABI error).

```bash
cd native/embedder-ane
python3.12 -m venv .venv
source .venv/bin/activate
pip install -r requirements.txt
```

Model files are not committed (see `.gitignore`). The two 8-bit `.mlpackage`s in `models/` were produced by loading `neuradex/Qwen3-Embedding-0.6B-CoreML-ANE`'s `packages/b1_s128.mlpackage` and `b1_s512.mlpackage` and applying:

```python
import coremltools as ct
import coremltools.optimize.coreml as cto

model = ct.models.MLModel("b1_s128.mlpackage")
config = cto.OptimizationConfig(
    global_config=cto.OpLinearQuantizerConfig(
        mode="linear_symmetric", dtype="int8", weight_threshold=512
    )
)
cto.linear_quantize_weights(model, config=config).save("models/qwen3-b1_s128-8bit.mlpackage")
```

and the same for `b1_s512`. `models/tokenizer.json` is the `tokenizer/tokenizer.json` from that same repository.

## Running

```bash
cd native/embedder-ane
source .venv/bin/activate
uvicorn server:app --host 0.0.0.0 --port 8086
```

Startup takes 30–60 s while Core ML compiles both graphs for the Neural Engine; `uvicorn` prints nothing until that is done, then `Application startup complete`. `GET http://localhost:8086/health` confirms it is up.

## API

```bash
curl -s localhost:8086/health
curl -s -XPOST localhost:8086/embed/document -H 'content-type: application/json' -d '{"text":"Wireless noise-cancelling headphones."}'
curl -s -XPOST localhost:8086/embed/query -H 'content-type: application/json' -d '{"text":"best headphones for travel"}'
```

Response: `{"values": [...], "model_used": "Qwen/Qwen3-Embedding-0.6B", "truncated": false}`. Empty text is `400`; errors are `{"error": "..."}` with a non-2xx status — the shape `knowledge-base`'s `embedder_client.rs` expects.

No request log — unlike `llm-router`'s `requests`/`request_payloads` tables — because a local, free, unmetered forward pass has nothing to audit.
