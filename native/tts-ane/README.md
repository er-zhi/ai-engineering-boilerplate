# Qwen3-TTS 0.6B on Apple Neural Engine

Experimental local English/Serena speech synthesis, Core ML export, KV state
reuse and raw PCM streaming. This is a development checkpoint, not a
production release. No prerecorded audio is used to answer requests.

## What is verified

- Stateful talker, fused residual predictor and history-preserving decoder
  pass the strict ANE-preferred compute-plan check. This is not hardware tracing.
- The optional 16-position initial KV cache migrates to the 128-position
  continuation cache. One 49-frame utterance matches full-cache generation
  exactly in audio codes, PCM and EOS.
- 100 warm loopback requests with the ordinary cache: first PCM body byte
  p50 58.31 ms / p95 64.69 ms; every complete response matched reference PCM.
- Short-cache timing is not a confirmed speedup: the last warm internal run
  measured p50 59.39 ms / p95 107.23 ms.

These measurements exclude fresh-text preparation and cold model loading.
First PCM is not necessarily audible speech. The 30 ms p95 goal is NOT met.
Current decoder output still differs from source FP32 on identical codes
(peak 0.02573 versus the 0.02 diagnostic threshold). KV parity does not
resolve this pre-existing quality gap. 0.6B CustomVoice does not expose native
emotion/style instructions; speaker selection is not emotion direction.

Local evidence lives in ignored reports/:
qwen06-safe-down-http-100.json, qwen06-short-kv-stream-01.prefix-parity.json,
qwen06-short-kv-stream-01.benchmark.json, and
quality-w8-safe-down-fp16-control-01/comparison.json.
Approved listening references remain in reports/quality-comparison-01/.
Do not overwrite them.

## Environment

Run commands from native/tts-ane. Requires Apple Silicon, macOS 15+,
Xcode command-line tools and Python 3.12. Tested locally with Core ML tools 9.0.
Torch 2.7.1 is pinned but triggers a Core ML compatibility warning.

```sh
python3.12 -m venv qwen-env
qwen-env/bin/pip install -r requirements-qwen.txt
xcrun swiftc -parse-as-library ane_gate.swift -o /tmp/ane-gate
```

Obtain Qwen/Qwen3-TTS-12Hz-0.6B-CustomVoice from its official Hugging Face
repository, including speech_tokenizer/, into
../tts-bakeoff/qwen06-customvoice. Weights, compiled models, virtual
environments, upstream checkouts, WAV and generated reports are not in git.
The qwen-tts source revision is pinned in requirements-qwen.txt.

## Build components

Exporters refuse to overwrite existing candidates. Use new paths for repeats.

```sh
qwen-env/bin/python export_stateful_predictor.py ../tts-bakeoff/qwen06-customvoice --talker --output models/qwen06-stateful-fp16/qwen06_cached_block0.mlpackage
qwen-env/bin/python export_stateful_predictor.py ../tts-bakeoff/qwen06-customvoice --talker --capacity 16 --output models/qwen06-stateful-short16-fp16.mlpackage
qwen-env/bin/python export_fused_predictor.py ../tts-bakeoff/qwen06-customvoice --codes 15 --float-selection --growing-cache --batch-prefix --output models/qwen06-fused15-batch-prefix-fp16.mlpackage
```

export_streaming_decoder.py validates FP32 recurrence before exporting.
Use its --explicit-state mode; it preserves sliding attention and convolution
history. Bootstrap the source-code fixture without any previously exported models:

```sh
qwen-env/bin/python generate_source_fixture.py ../tts-bakeoff/qwen06-customvoice --output reports/source-greedy.json
qwen-env/bin/python export_streaming_decoder.py ../tts-bakeoff/qwen06-customvoice reports/source-greedy.json --explicit-state --report reports/decoder-export.json --output models/streaming-decoder-explicit-noslice-fp16.mlpackage
```
Do not replace it with a repeatedly reset rolling window.

For the measured mixed-W8 candidate, first capture real predictor inputs
using the generation command below with the FP16 predictor path substituted,
without --short-talker-package, adding
--capture-predictor-inputs reports/qwen06-predictor-real-inputs.npz.
Then:

```sh
qwen-env/bin/python export_fused_predictor.py ../tts-bakeoff/qwen06-customvoice --codes 15 --float-selection --growing-cache --batch-prefix --outlier-scale 256 --validation-samples reports/qwen06-predictor-real-inputs.npz --output models/qwen06-fused15-outlier256-fp16.mlpackage
qwen-env/bin/python quantize_qwen_block.py models/qwen06-fused15-outlier256-fp16.mlpackage --output models/qwen06-outlier256-w8-down-fp16.mlpackage --matrices-only --keep-predictor-heads --keep-predictor-embeddings --keep-weight core_model_layers_2_mlp_down_proj_weight_to_fp16
qwen-env/bin/python rescale_predictor_projection.py models/qwen06-outlier256-w8-down-fp16.mlpackage models/qwen06-outlier256-w8-safe-down.mlpackage --allow-subnormal-rounding
```

The rescaling addresses observed numerical overflow; it explicitly allows
small subnormal constant rounding and is not a lossless-quality guarantee.
INT8 refers to selected weights, not PCM. Failed activation-quantization and
re-prefill options remain diagnostic only, never the runtime default.

## Generate and verify KV migration

With the above models and the exported history decoder available:

```sh
qwen-env/bin/python voice_stream.py ../tts-bakeoff/qwen06-customvoice models/qwen06-stateful-fp16 --gate /tmp/ane-gate --output reports/kv-check.wav --speaker Serena --model-prefix qwen06 --block-count 1 --compiled-dir models/compiled-cache --predictor-package models/qwen06-outlier256-w8-safe-down.mlpackage --decoder-package models/streaming-decoder-explicit-noslice-fp16.mlpackage --experimental-history-decoder --experimental-prefix-state --short-talker-package models/qwen06-stateful-short16-fp16.mlpackage --verify-prefix-state --first-chunk-trials 100
```

The short-cache mode is opt-in. Its 9-token invariant language/speaker prefix
is reused under a serial-session lock; generated future positions are masked.
At position 16, populated K/V is copied into the full state without dropping
history. Prefix identity changes require rebuilding the prepared voice.
This is not a multi-user cache or a general fresh-text serving API.

## Stream and measure

Add --serve 8765 to the generation command. Each GET /stream synthesizes the
configured text anew. The server binds only to 127.0.0.1 and processes requests
serially. Response: HTTP chunked raw PCM16 little-endian, mono, 24 kHz.
The HTTP server is a diagnostic, not a hardened production endpoint.

```sh
qwen-env/bin/python test_voice_stream_client.py --port 8765 --output reports/client.wav
qwen-env/bin/python benchmark_stream_http.py --reference-wav reports/kv-check.wav --output reports/http-benchmark.json
qwen-env/bin/python compare_qwen_quality.py ../tts-bakeoff/qwen06-customvoice reports/kv-check.json --output reports/quality-check --decode-only --source-fp16-control
```

The HTTP benchmark checks all PCM bytes against the supplied WAV, not merely
the first packet. The source-quality control reports errors; exit zero is NOT
a passing perceptual-quality gate. Its CPU reference is not a runtime fallback.

## Checks and next work

```sh
qwen-env/bin/python -m unittest test_voice_sampling test_ane_gate_report test_activation_boundaries
```

Next: controlled short/full KV timing, broader utterance/state-isolation tests,
fix source-decoder discrepancies, optimize the residual predictor, and include
fresh-text preparation and client delivery in end-to-end p95. Do not claim
30 ms, native emotion control, or lossless quality from component benchmarks.
