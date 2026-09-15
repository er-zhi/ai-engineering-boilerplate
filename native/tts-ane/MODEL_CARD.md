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
- streaming
- python
---

# Qwen3-TTS 0.6B Serena — sub-50 ms warm first PCM on Apple Neural Engine

An experimental, unofficial Apple Silicon port of
[Qwen3-TTS-12Hz-0.6B-CustomVoice](https://huggingface.co/Qwen/Qwen3-TTS-12Hz-0.6B-CustomVoice).
It exposes one warm English Serena voice through a transport-neutral Python API
that yields 24 kHz mono PCM16 chunks as soon as they are generated.
The complete download is approximately 2.50 GB and includes the Core ML graphs,
tokenizer, Serena frontend tables, embeddable runtime, and one audio sample.
Host embedding tables use FP16 plus sparse FP32 corrections; every reconstructed
value and the complete sample WAV match the FP32 frontend exactly.

The cached, full-prefill and long-context talker graphs share one immutable
887 MB FP16 weight blob. The first load materializes the omitted prefill and
long-context package copies as hard links when the cache is on the same
filesystem, with a safe copy fallback. No model values are changed and the
source snapshot may remain read-only.

The default startup path uses a separate 6-bit palettized first-frame-only
prefill graph, then rebuilds the exact FP16 continuation KV after the first PCM
chunk has been yielded. This moves the expensive exact prefill out of the
first-byte critical path without caching generated audio or changing the
completed waveform.

[Listen to the Serena sample](https://huggingface.co/erjigit17/Qwen3-TTS-0.6B-ANE/resolve/main/samples/serena.wav).

## Key highlights

- **1.418x real-time median** for a complete 4.08-second utterance on the tested
  M4 MacBook Air; all seven measured runs finished with identical PCM.
- **44.88 ms p50 / 47.75 ms p95 to the first PCM body byte** across 300 warm
  fixed-prepared-prompt runs. This is an in-process model boundary; text
  preparation, transport and audible speech onset are outside this measurement.
- **Production KV-state support.** `use_prefix_kv=True` reuses the invariant
  nine-token voice prefix and restores an immutable state after every completed
  or cancelled request. It is independent of the default dual-prefill path.
- **Quality-first FP16 release.** Packaging, streaming, KV-state migration and
  frontend compaction were accepted only after exact code/PCM comparisons.
- **ANE-admitted neural graphs.** The strict Core ML compute-plan gate reported
  every neural operation as ANE-preferred. Tokenization, orchestration and PCM
  serialization still run on the host.
- **Approximately 2.50 GB transport-neutral bundle.** It includes the tokenizer, runtime and
  model graphs, without PyTorch, Transformers, gRPC, Xcode or duplicated talker
  weights.

## Quality-first objective and exact-parity evidence

The objective of this port is **real-time synthesis without changing the
accepted Serena voice**. It optimizes execution, streaming and packaging rather
than treating the smallest possible download as the primary goal. A compressed
candidate is not promoted merely because it is faster or smaller; it must first
preserve the listening quality of the FP16 release baseline.

The following strict comparisons were run. Here, *exact* means identical audio
codes or identical PCM bytes—not a rounded similarity score:

| Check | Exact result |
|---|---|
| Reconstructed host frontend versus the original FP32 frontend tables | Every reconstructed embedding value matched exactly. |
| Deduplicated cached, prefill and long-context talker packages | Their 887 MB FP16 weight blobs were byte-identical; both duplicate copies were omitted without changing a model value. |
| FP16 decoder-output boundary optimization | All 51 PCM frames and the complete WAV remained byte-identical: `c5c4f657588f9d6ec8acbde297c890e0f131734f359c596e550d3b3f9de592a7`. |
| Incremental text split into 1-, 7- and 19-character pieces | Every run produced the same 51 audio-code frames and PCM as the whole-text request: raw-PCM SHA-256 `52540b75be148e9fae0b691d0bead1627c82254b5e05b0f5250fcf8fb540b782`. |
| Streaming transport versus the same runtime's whole-text generation | All 51 chunks, totaling 195,840 PCM bytes, matched exactly. |
| Short initial KV state followed by migration to the full state | One complete 49-frame utterance matched fresh sequential ANE prefill in codes, PCM and EOS. |
| Repeated warm requests and state restoration | 100 complete loopback responses matched reference PCM exactly; cancellation/reuse tests also restored deterministic output. |
| LUT6 first-frame prefill followed by exact FP16 continuation prefill | Five texts from 51 to 244 frames matched full-prefill codes and PCM exactly; repeated runs and cancellation at multiple frame positions also passed. The 51-frame WAV remained `c5c4f657588f9d6ec8acbde297c890e0f131734f359c596e550d3b3f9de592a7`. |

These checks protect against quality regressions caused by streaming boundaries,
KV-state reuse, packaging deduplication and host/Core ML precision conversions.
They do **not** mean that every intermediate tensor is bit-identical across
PyTorch and Core ML. The current Core ML decoder has a measured numerical
difference from the upstream FP32 decoder on identical codes, so this card does
not claim full bitwise identity with upstream PyTorch. Listening approval and
the packaged FP16 WAV remain the release-quality reference. Experimental Q8
talker builds are excluded because they changed the perceived voice character.

## Performance evaluation

Tested on a 32 GB M4 MacBook Air, macOS 26.5, Xcode 27, Core ML Tools 9.0.
The throughput result used two warmups followed by seven measured generations
of `I'm sorry about the charge. I'll fix it for you.`. A separate startup test
used five warmups and 300 measured generations of the already prepared fixed
prompt. Its first chunk was identical after every session reset.

| Metric | Result |
|---|---:|
| Warm prepared prompt to first PCM, p50 | 44.88 ms |
| Warm prepared prompt to first PCM, p95 | 47.75 ms |
| First-to-second PCM interval, p50 | 85.93 ms |
| First-to-second PCM interval, p95 | 91.26 ms |
| Complete generation, median | 2.878 s |
| Complete throughput, median | 1.418x real-time |
| Slowest measured throughput | 1.245x real-time |
| Deterministic raw PCM | Yes, 7/7 identical |
| Peak Python RSS | 1.44 GB |

Model loading, Core ML compilation, fresh text preparation, transport and
audible speech onset are excluded from first-PCM timing. One PCM chunk contains 80 ms of audio. One
synthesis is supported at a time. Peak Python RSS excludes OS-managed Core
ML/ANE allocations and the on-disk compiled cache. Earlier loopback measurements
are retained in the source repository, but are not mixed into this in-process
benchmark.

All selected Core ML graphs passed this project's strict compute-plan gate:
100% of reported neural operations preferred ANE, with zero reported CPU/GPU-
preferred neural operations. Tokenization, array handling, transport adapters
and Core ML control remain host work; this is not a zero-CPU system.

## CPU versus ANE

The same-graph CPU-only comparison is currently **not reportable as a speedup**.
On the tested macOS 26.5 / Core ML Tools 9.0 stack, all packages load with
`ComputeUnit.CPU_ONLY`, but the first explicit-history decoder prediction
reproducibly terminates the native process with `SIGSEGV` (exit 139) inside
`coremltools.models.model._get_predictions`. The ANE run completes and remains
deterministic. The benchmark therefore fails closed instead of publishing a
fabricated CPU number; CPU-only execution is disabled unless the caller opts
into the known crash risk explicitly.

Apple documents `CPU_ONLY` as a supported compute-unit restriction, so this is
tracked as a runtime-compatibility defect rather than evidence that CPU is
intrinsically incapable of running the architecture. A CPU/ANE speedup will be
published only after the CPU decoder completes the same prompt, graph set,
warmup policy and trial count.

## Precision and quantization controls

Quantization changes are evaluated under stricter controls than file size or
word-error rate alone. Each candidate is compared with the accepted FP16 Core
ML baseline using exact generated audio codes, first differing frame, raw-PCM
hashes, repeatability and listening checks for perceived age, timbre, prosody,
pronunciation and artifacts.

The tested talker Q8 candidate reached approximately 1.43x real-time, but only
3 of 50 complete code frames and 36 of 50 semantic codes matched the FP16 run;
listening also changed the perceived voice character. It is therefore not the
default release. An 8-bit grouped-palettization experiment was also rejected:
it expanded one package to 7 GB and its strict compute plan reported 0% ANE
preference. These failed candidates are not shipped.

“Bit-exact” in this card always names its boundary. The accepted claims cover
the compacted frontend, deduplicated Core ML weights, streaming boundaries,
KV-state migration, and the packaged sample relative to the approved Core ML
reference. They do not claim that the entire port is bit-identical to upstream
PyTorch.

## Download and run

Requires Apple Silicon macOS and Python 3.10–3.13. Python 3.12 and 3.13 are
validated environments for this release. A clean Python 3.13 installation with
only the three declared runtime packages reproduced the packaged sample WAV
byte-for-byte. Xcode is not required at runtime.

```bash
python3.12 -m pip install --upgrade huggingface_hub
hf download erjigit17/Qwen3-TTS-0.6B-ANE --local-dir Qwen3-TTS-0.6B-ANE
cd Qwen3-TTS-0.6B-ANE
python3.12 -m venv .venv
. .venv/bin/activate
pip install -r requirements.txt
python verify_install.py
python example.py "I'm sorry about the charge. I'll fix it for you." --output answer.wav
```

Run `python verify_install.py --checksums` when a full byte-level download
integrity check is wanted before the first Core ML compilation.

Embed streaming synthesis directly. This default constructor selects the
quality-verified dual-prefill startup path:

```python
from qwen3_tts_ane import Qwen3TTSANE

voice = Qwen3TTSANE()
for chunk in voice.stream("I'm sorry about the charge. I'll fix it for you."):
    send_or_play(chunk.pcm_s16le)
```

`voice.synthesize(text)` is available when a complete PCM byte string is more
convenient. Construct `Qwen3TTSANE(use_prefix_kv=True)` to enable reusable model
KV state; this caches Transformer state, never generated audio. That option
replaces dual prefill for a request and remains available for deployments that
prefer cross-request prefix reuse. Transport adapters such as gRPC belong in the application
and are maintained in the linked source repository rather than this model
bundle. Component paths and tensor/audio contracts are recorded in
`model-config.json`. Run `shasum -a 256 -c SHA256SUMS` to verify the download.

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
