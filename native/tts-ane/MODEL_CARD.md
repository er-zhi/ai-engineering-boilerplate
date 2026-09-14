---
license: apache-2.0
language:
- en
pipeline_tag: text-to-speech
library_name: coremltools
base_model: Qwen/Qwen3-TTS-12Hz-0.6B-CustomVoice
tags:
- qwen3-tts
- apple-silicon
- coreml
- ane
- grpc
- streaming
---

# Qwen3-TTS 0.6B Serena — 1.3x real-time on Apple Neural Engine

An experimental, unofficial Apple Silicon port of
[Qwen3-TTS-12Hz-0.6B-CustomVoice](https://huggingface.co/Qwen/Qwen3-TTS-12Hz-0.6B-CustomVoice).
It serves one warm English Serena voice through bidirectional gRPC. Clients may
send text incrementally while receiving 24 kHz mono PCM16 chunks.
The complete download is approximately 3.96 GB and includes the Core ML graphs,
tokenizer, Serena frontend tables, runnable server/client, and one audio sample.
Host embedding tables use FP16 plus sparse FP32 corrections; every reconstructed
value and the complete sample WAV match the FP32 frontend exactly.

## Measured result

Tested on a 32 GB M4 MacBook Air, macOS 26.5, Xcode 27, Core ML Tools 9.0.
Thirty warm loopback gRPC trials including new-text processing measured first
PCM at 70.6 ms p50 and 133.7 ms p95. Three complete 4.08-second generations
finished at 1.299x, 1.328x and 1.350x real-time (1.328x median). Model loading,
Core ML compilation and audible speech onset are excluded from first-PCM timing.
One PCM chunk contains 80 ms of audio. One synthesis is supported at a time.
Observed Python RSS after one complete request was approximately 385 MiB;
this excludes OS-managed Core ML/ANE allocations and the on-disk compiled cache.

All selected Core ML graphs passed this project's strict compute-plan gate:
100% of reported neural operations preferred ANE, with zero reported CPU/GPU-
preferred neural operations. Tokenization, array handling, gRPC and Core ML
control remain host work; this is not a zero-CPU system.

## Run

Requires Apple Silicon, Python 3.12 and Xcode command-line tools.

```bash
python3.12 -m venv .venv
. .venv/bin/activate
pip install -r requirements-runtime.txt
python serve.py
```

In another terminal:

```bash
. .venv/bin/activate
python client.py "I'm sorry about the charge. I'll fix it for you." --output answer.wav
```

The protocol is `tts.v1.Speech/Synthesize` in `tts_stream.proto`. Half-close the
request stream when text is complete; cancel the RPC to immediately stop the
generation. The server binds only to `127.0.0.1` and intentionally has no TLS or
authentication.

## Scope and limitations

- English and the Serena female voice only in this release.
- Greedy generation; no voice cloning.
- The packaged runtime does not expose reliable instruction/emotion control.
- Maximum 502 audio frames (about 40 seconds before an earlier natural EOS).
- Warm throughput is faster than playback on the tested M4, but not certified
  across other Apple chips, power modes or concurrent workloads.
- This is a developer preview, not an official Qwen release or production SLA.

The host assets were extracted from upstream revision
`85e237c12c027371202489a0ec509ded67b5e4b5`; conversion source is maintained at
[er-zhi/ai-engineering-boilerplate](https://github.com/er-zhi/ai-engineering-boilerplate/tree/main/native/tts-ane).
Qwen3-TTS and this distribution use Apache-2.0. See `LICENSE` and the upstream
model card for details.
