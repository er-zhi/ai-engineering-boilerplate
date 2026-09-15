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

Follow-up controlled component comparison (100 alternating pairs, three texts,
10 warmups per model): short cache p50/p95 14.88/18.09 ms; full cache
16.29/20.72 ms. Short was faster in 99/100 pairs, with median paired saving
1.46 ms, and logits/hidden outputs matched exactly. Evidence:
reports/qwen06-short16-paired-cache-01.json. This supports a small first-step
benefit, not an end-to-end PCM speedup or the 30 ms target. The earlier full
stream timing remains as recorded; migration overhead still needs to be
included in sustained-stream comparisons.

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
repository, including speech_tokenizer/, and substitute its local path for
`/path/to/qwen06-customvoice` below. Weights, compiled models, virtual
environments, upstream checkouts, WAV and generated reports are not in git.
The qwen-tts source revision is pinned in requirements-qwen.txt.

## Build components

Exporters refuse to overwrite existing candidates. Use new paths for repeats.

```sh
qwen-env/bin/python export_stateful_predictor.py /path/to/qwen06-customvoice --talker --output models/qwen06-stateful-fp16/qwen06_cached_block0.mlpackage
qwen-env/bin/python export_stateful_predictor.py /path/to/qwen06-customvoice --talker --capacity 16 --output models/qwen06-stateful-short16-fp16.mlpackage
qwen-env/bin/python export_fused_predictor.py /path/to/qwen06-customvoice --codes 15 --float-selection --growing-cache --batch-prefix --output models/qwen06-fused15-batch-prefix-fp16.mlpackage
```

export_streaming_decoder.py validates FP32 recurrence before exporting.
Use its --explicit-state mode; it preserves sliding attention and convolution
history. Bootstrap the source-code fixture without any previously exported models:

```sh
qwen-env/bin/python generate_source_fixture.py /path/to/qwen06-customvoice --output reports/source-greedy.json
qwen-env/bin/python export_streaming_decoder.py /path/to/qwen06-customvoice reports/source-greedy.json --explicit-state --report reports/decoder-export.json --output models/streaming-decoder-explicit-noslice-fp16.mlpackage
```
Do not replace it with a repeatedly reset rolling window.

For the measured mixed-W8 candidate, first capture real predictor inputs
using the generation command below with the FP16 predictor path substituted,
without --short-talker-package, adding
--capture-predictor-inputs reports/qwen06-predictor-real-inputs.npz.
Then:

```sh
qwen-env/bin/python export_fused_predictor.py /path/to/qwen06-customvoice --codes 15 --float-selection --growing-cache --batch-prefix --outlier-scale 256 --validation-samples reports/qwen06-predictor-real-inputs.npz --output models/qwen06-fused15-outlier256-fp16.mlpackage
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
qwen-env/bin/python voice_stream.py /path/to/qwen06-customvoice models/qwen06-stateful-fp16 --gate /tmp/ane-gate --output reports/kv-check.wav --speaker Serena --model-prefix qwen06 --block-count 1 --compiled-dir models/compiled-cache --predictor-package models/qwen06-outlier256-w8-safe-down.mlpackage --decoder-package models/streaming-decoder-explicit-noslice-fp16.mlpackage --experimental-history-decoder --experimental-prefix-state --short-talker-package models/qwen06-stateful-short16-fp16.mlpackage --verify-prefix-state --first-chunk-trials 100
```

The short-cache mode is opt-in. Its 9-token invariant language/speaker prefix
is reused under a serial-session lock; generated future positions are masked.
At position 16, populated K/V is copied into the full state without dropping
history. Prefix identity changes require rebuilding the prepared voice.
This is not a multi-user cache or a general fresh-text serving API.

## Stream and measure

### CPU audit of the current runtime

The runtime is NOT an entirely ANE neural pipeline. Static inspection of
voice_stream.py and the pinned upstream generation code identifies:

| Work | Current execution | Remaining action |
| --- | --- | --- |
| Text tokenization, input validation | Host CPU during prepare | Keep host control separate from neural compute |
| Text embedding and learned text_projection | Embedding lookup stays on CPU; optional --text-projection-package moves learned projection to ANE-preferred graph | Experimental, generated output differs; not quality-promoted |
| Speaker/code embedding lookup | CPU during prepare and between frames | Fold lookup into adjacent ANE graphs if admitted |
| Codec codebook decode/output projection of all entries | PyTorch CPU at setup | Export derived constant tables at build time, or integrate decoder |
| Rotary values and attention/write masks | PyTorch/NumPy CPU at setup | Export constants or produce in-graph where beneficial |
| Talker, fused residual transformer and waveform decoder | ANE-preferred Core ML graphs | Placement check is not a hardware execution trace |
| Repetition penalty, vocabulary mask, semantic argmax/EOS | NumPy CPU each frame | Fuse with talker output while preserving exact selection semantics |
| Sixteen codebook lookups and their latent sum | NumPy CPU each frame | Fuse into history-preserving decoder |
| Audio-code embedding sum plus trailing text | NumPy CPU each continuation | Fuse into talker input |
| Explicit decoder state transfer and optional KV migration | Host bridge | Profile copies; stateful graph changes must preserve history |
| Finite checks, clipping, PCM16 conversion | NumPy CPU each frame | Keep safety checks; distinguish serialization from model inference |
| Scheduling, HTTP, file writes, model loading/compilation | Host CPU/OS | Not neural inference; cannot describe the whole app as ANE-only |

These locations are source evidence, not measured CPU percentages. prepare()
is excluded from existing first-PCM benchmarks, including learned text
projection work. The older non-fused predictor path additionally selects each
residual code on CPU; it is not the measured fused candidate. CPU reference
decoding in quality tools is intentional and not a production fallback.
The direct embedding gather pilot fails strict placement (six CPU-preferred
operations); it must not be used to claim a completed ANE migration.

Add --serve 8765 to the generation command. Each GET /stream synthesizes the
configured text anew. The server binds only to 127.0.0.1 and processes requests
serially. Response: HTTP chunked raw PCM16 little-endian, mono, 24 kHz.
The HTTP server is a diagnostic, not a hardened production endpoint.

```sh
qwen-env/bin/python test_voice_stream_client.py --port 8765 --output reports/client.wav
qwen-env/bin/python benchmark_stream_http.py --reference-wav reports/kv-check.wav --output reports/http-benchmark.json
qwen-env/bin/python compare_qwen_quality.py /path/to/qwen06-customvoice reports/kv-check.json --output reports/quality-check --decode-only --source-fp16-control
```

The HTTP benchmark checks all PCM bytes against the supplied WAV, not merely
the first packet. The source-quality control reports errors; exit zero is NOT
a passing perceptual-quality gate. Its CPU reference is not a runtime fallback.

## Checks and next work

Before the next commit, run a broader stability matrix: short call-center
replies, long multi-clause sentences, technical vocabulary (idempotency,
PostgreSQL, OAuth, Kubernetes), dates/currency/decimals, acronyms, punctuation
and pauses. Exercise supported speaker/text/frame-limit settings, repeated
sessions, cancellation, KV isolation and migration boundaries. Check finite
PCM, correct length, natural EOS versus truncation, sustained delivery,
source-decoder error and listening quality. Compare cold/post-idle/warm timing
separately. Do not invent emotion controls unsupported by 0.6B CustomVoice.
This matrix is required future work, not a claim of completed stability tests.

### No cross-request prefix reuse baseline

The current optimization track disables --experimental-prefix-state and
--short-talker-package. Ordinary within-utterance KV state remains enabled.
100 warm trials with sequential fresh prefill: first PCM p50/p95
206.44/212.02 ms. With --prefill-packages models/qwen06-full-fp16, fresh
batched prefill reduces that to 64.39/69.67 ms (prefill p50 25.50 ms).
Evidence: reports/qwen06-no-prefix-baseline-01.benchmark.json and
reports/qwen06-no-prefix-batched-01.benchmark.json. Both exclude text
preparation/transport, use no audio cache, and regenerate the prefix.
The batched run ends naturally at 48 frames versus 49 for sequential prefill;
this is not an exact-output optimization or established quality preservation.
Use it as an experimental timing baseline, not a promoted listening build.

Last-token prefill experiment: export_voice_prefill_cache.py now supports
--all-layers --length 10 --last-token-only. It preserves all K/V but computes
the final normalization/head only for the last token. Runtime rejects padded
or multi-block use of this output contract. The complete 48-frame WAV and
audio codes match the preceding batched-prefill run exactly (SHA-256
377ce6a9072a15c99f3c27fba623dd415a639decf544cc6ea058ac3f6d5bccb1).
No performance improvement is established: the separate 100-trial run gave
first PCM p50/p95 69.60/97.86 ms, with elevated timings across all components;
prefill p50 remained 25.62 ms. Evidence: reports/qwen06-last-token-prefill-01.*.
This is not a fix for the existing source-decoder discrepancy.

Embedding-lookup pilot: export_fused_predictor.py --gather-embeddings retains
floating-point code selection but replaces the one-hot/table matrix product
with a direct row lookup. The two-code probe matches all 116 FP32 source codes
on 58 inputs before export. Strict ANE admission fails: 99.18% preferred ANE,
6 CPU-preferred operations. No latency result is accepted; this diagnostic
option is not integrated into the full predictor or runtime defaults.
Reproduce with --codes 2 --float-selection --growing-cache --batch-prefix
--gather-embeddings --validation-samples reports/qwen06-predictor-real-inputs.npz,
then run ane_gate on the exported package.

Packed QKV and gate/up experiment: all 870 FP32 source codes agree before
export, and all 870 candidate codes agree with the original Core ML FP16
predictor after export. Strict placement passes. Alternating 100-trial
predictor-only p50/p95 is 52.24/69.20 ms versus reference 48.53/62.84 ms.
No speed gain; not a default. Evidence:
reports/qwen06-packed-projections-fp16-benchmark.json.

### Experimental ANE text projection and stability evidence

```sh
qwen-env/bin/python export_text_projection.py /path/to/qwen06-customvoice --output models/qwen06-text-projection32.mlpackage --report reports/qwen06-text-projection32.json --gate /tmp/ane-gate-cost
```

Pass --text-projection-package models/qwen06-text-projection32.mlpackage
to voice_stream.py to replace the learned CPU projection. The adapter splits
text into independent 32-token batches and removes padding; it does not split
speech or reuse generated audio. All three graph operations prefer ANE.
Projection-only 100-trial p50/p95: 0.528/0.585 ms. FP32 source peak/RMS error:
0.04017/0.002155. This is NOT a lossless conversion. No physical hardware
execution claim follows from a preferred-placement report alone.

The integrated no-prefix-reuse run produced 51 frames with natural EOS,
versus 48 with CPU text projection. Its 100 warm prepared-prompt first-PCM
p50/p95 is 65.95/67.86 ms, excluding preparation and transport. First PCM
is identical after resets, but source/perceptual quality is unapproved.
Evidence: reports/qwen06-ane-text-projection-01.*.

```sh
qwen-env/bin/python check_stream_stability.py reports/qwen06-ane-text-projection-01.json --gate /tmp/ane-gate-cost --output reports/stability-ane-text-projection-01
qwen-env/bin/python -m unittest test_text_projection test_voice_sampling test_ane_gate_report test_activation_boundaries
```

Five real-text cases passed the executed repeatability checks: complete PCM
and codes repeat exactly; closing generators after frames 1/16/73 (where
available) preserves the next request's first chunk; returning to the original
text after other texts reproduces its complete PCM. These are local generator
cancellations, NOT HTTP disconnect tests. Short, technical and punctuation
cases end naturally at 51/90/99 frames. Number/date and long-sentence cases
truncate at 118 frames (9.44 seconds). Thus long-utterance support is NOT done.
WAVs and incremental report are under reports/stability-ane-text-projection-01/.
18 unit tests passed; they do not validate pronunciation, emotion, listening
quality, cold timing, all speakers, HTTP cancellation or concurrent requests.

Hardware trace: reports/ane-runtime-01.trace (Xcode 27 Core AI, launched Python
PID 4519, exit 0) records ANE Prediction intervals for all five candidate
components: 4 text-projection calls, 1 batched prefill, 51 predictor calls,
51 decoder calls and 51 cached-talker calls. This confirms real ANE activity
for these model labels, not exclusive ANE execution of the whole application.
The trace's ANE table has no per-row PID; no per-operation no-fallback claim
is made. Instrumented durations must not replace uninstrumented p95 trials.

```sh
xcrun xctrace export --input reports/ane-runtime-01.trace --xpath '/trace-toc/run[@number="1"]/data/table[@schema="ane-hw-intervals"]' --output reports/ane-runtime-01-hardware.xml
xcrun xctrace export --input reports/ane-runtime-01.trace --xpath '/trace-toc/run[@number="1"]/data/table[@schema="time-profile"]' --output reports/ane-runtime-01-cpu.xml
qwen-env/bin/python summarize_ane_trace.py reports/ane-runtime-01-hardware.xml reports/ane-runtime-01-cpu.xml --pid 4519 --output reports/ane-runtime-01-summary.json
```

Capture using xctrace record --template 'Core AI' --time-limit 45s --output
NEW.trace --no-prompt --launch -- ABSOLUTE_PYTHON voice_stream.py [arguments].
Use new paths and the captured target PID when reproducing. CPU samples during
synthesis include Core ML FP16-to-FP32 array conversion (57 ms sample weight),
memmove (56 ms) and finite checks (24 ms); weights are not elapsed time.
The pinned coremltools 9.0 CompiledMLModel.predict calls
MLModel._update_float16_multiarray_input_to_float32, so merely passing FP16
NumPy arrays does not establish zero-copy or remove boundary conversion.

Source-decoder comparison of the 51-frame candidate still fails the existing
0.02 peak-error diagnostic: peak 0.02573, RMS 0.001390, SNR 30.92 dB.
PyTorch FP16 control on identical codes has peak 0.002530 and SNR 51.99 dB.
Evidence: reports/quality-ane-text-projection-01/comparison.json. The ANE text
projection did not resolve the pre-existing decoder discrepancy.

FP16 decoder-output experiment: convert_output_precision.py uses the public
change_input_output_tensor_type API. All 51 frames match the preceding WAV
exactly (SHA-256 c5c4f657588f9d6ec8acbde297c890e0f131734f359c596e550d3b3f9de592a7).
No established speed improvement: separate p50/p95 65.58/68.17 ms, with a
unit-test process overlapping part of the measurement. Python still promotes
FP16 inputs. Runtime converts PCM to FP32 before PCM16 scaling to preserve
rounding; safety checks remain. Evidence: reports/qwen06-fp16-output-decoder-01.*.

Compact prefill experiment: --all-layers --length 10 --last-token-only
--compact-kv returns only populated K/V (2.19 MiB rather than 28 MiB FP32).
The ordinary 128-position state is still allocated for continuation; nothing
is reused between requests. On 100 alternating synthetic-input trials against
the last-token-only padded export, p50/p95 including output finite checks is
19.21/22.87 ms versus 31.44/38.18 ms; median paired saving 10.64 ms.
Hidden/logits/values match, but keys differ by up to 7.6294e-6. This is NOT
an exact-output promotion. Full audio codes/WAV also differ. Full first-PCM
p50/p95 was 59.00/83.60 ms in a variable run; 30 ms is not reached. After that
process exited successfully, the native runtime printed an ANE E5 recompile
diagnostic; a subsequent fresh-process paired run passed without that message.
Do not treat one successful generation as completed stability validation.
Evidence: reports/compact-prefill-paired-01.json and
reports/qwen06-compact-last-prefill-01.*.

```sh
qwen-env/bin/python -m unittest test_voice_sampling test_ane_gate_report test_activation_boundaries
```

## Release cleanup requirement

Fresh-text endpoint: POST /stream accepts JSON {"text":"...","max_frames":64}.
GET remains a configured-text diagnostic. The server retains a frontend after
startup rather than reloading the checkpoint per request; unused PyTorch
talker/predictor/decoder layers are removed from that retained object, while
embedding lookup/tokenizer and prompt construction remain host-side. This is
still a development dependency on the upstream checkpoint, NOT the minimal HF
inference distribution. There is no saved-audio or cross-request prefix cache.

```sh
qwen-env/bin/python benchmark_fresh_text.py --reference-wav reports/qwen06-fresh-text-server-01.wav --output reports/fresh-text-client-01.json
```

With the existing FP16 prefill, W8 safe-down predictor, FP16 explicit decoder,
ANE text projection and optional long-512 talker loaded, 100 new-text POSTs
(four rotating texts) give client first-PCM p50/p95 70.66/76.55 ms. This includes
TCP/HTTP and preparation; startup model loading is excluded. Preparation
p50/p95 is 5.04/7.11 ms. Fresh-text full PCM matches the configured-text WAV;
after a real HTTP disconnect, the next complete request matches again.
Five malformed/invalid JSON-field cases returned 400. Evidence:
reports/fresh-text-client-01.json. Timed requests ask for one audio frame;
they isolate startup and do not certify sustained delivery. This endpoint
accepts a COMPLETE text and streams AUDIO; incremental text input remains
unimplemented. First PCM can precede audible speech and is not a 30 ms result.

The extended-state stability matrix additionally passed complete-repeat and
generator-cancellation checks at frames 1/16/73/118/120 for number/date and
long-sentence cases. They end naturally at 160 and 244 frames, respectively.
Evidence: reports/stability-long512-01/report.json. This does not certify all
voices, all texts, perceptual quality or every migration boundary.

Current recovery evidence: the existing decoder compiled artifact reproducibly
failed MLComputePlan structure inspection while a fresh compile of the same
source package passed all 869 operations. admit_ane_package now serializes
access to each compiled-cache entry, preserves a rejected entry as .rejected,
rebuilds from the package once, and requires strict readmission. It never
accepts CPU fallback or repeats recovery indefinitely. The underlying native
cache invalidation cause is not established. The reproduced entry recovered
successfully and was used for the long-utterance run. 20 unit tests passed,
including recovery, preservation and rejection of a newly invalid compilation.

Long-utterance candidate: export_stateful_predictor.py --talker --capacity 512
creates an optional --long-talker-package. Runtime starts with the original
128-position model and migrates K/V at position 128, with no cross-request
reuse. First 118 PCM frames match the earlier truncated long-text WAV exactly
(453120 compared bytes). The same text now ends naturally at 244 frames,
19.52 seconds of audio, in 16.73 seconds of synthesis. This is one successful
utterance, not broad quality/migration certification. Evidence:
reports/qwen06-long512-01.{json,wav}; broader repetition/cancellation checks
are recorded separately under reports/stability-long512-01/.

The maintained native/ tree contains only embedder-ane/ and tts-ane/. Conversion
commands accept an independently downloaded official checkpoint; no tracked
source hard-codes an adjacent experiment directory. Runtime inference uses the
exported frontend bundle and does not load the upstream checkpoint.

The HF inference distribution must contain only the selected Qwen ANE models,
minimal runnable inference code, dependency/version metadata, licenses and a
clear installation/streaming example. Do not ship experimental graphs, all
benchmark WAVs, trace bundles, virtual environments or a duplicate upstream
repository. Preserve reproducible conversion instructions/source pins in the
development repository, separate from the minimal inference download.

Release acceptance includes a clean install outside this checkout, offline
inference after model download, no adjacent checkpoint dependency, measured download
size and resident memory, and a client first-PCM test. The model card must name
the tested Apple Silicon hardware/macOS, warm versus cold timing, p50/p95,
remaining host operations, supported voice controls and limits. ANE requires
Apple hardware; this is not an ordinary Linux/server deployment. Do not claim
30 ms, zero CPU, or native emotion tags without matching evidence.

Next: long-utterance migration validation, broader settings/HTTP isolation tests,
fix source-decoder discrepancies, optimize the residual predictor, and include
fresh-text preparation and client delivery in end-to-end p95. Do not claim
30 ms, native emotion control, or lossless quality from component benchmarks.

## Hugging Face publication plan

Before the next public release, rewrite the model card to the same professional
level of detail as Arm's Qwen3-TTS bundle. Keep the distribution transport-neutral
and organize the card as: Key Highlights, Quality Evaluation, Performance
Evaluation, CPU versus ANE, Runtime Architecture, Precision and Quantization,
Installation, Streaming API, Reproduction, Checksums, and Limitations.

Publication gates:

- Demonstrate more than 1.4x real-time median throughput on the named Apple
  Silicon device with repeated complete utterances.
- Demonstrate 50 ms or less warm time to the first PCM body byte for the stated
  scope. Always publish p50 and p95, trial/warmup counts, chunk size, and every
  excluded stage; do not relabel internal readiness as network or audible latency.
- Run CPU-only and CPU+ANE on the same Core ML graphs, prompt, process isolation,
  warmup policy and number of trials. Publish first-PCM, full-stream time, RTF,
  peak RSS and PCM equality/difference for both backends.
- Treat the approved FP16 Serena output as the release voice reference. For every
  quantized candidate, record exact audio-code agreement, first differing frame,
  PCM hashes, numerical decoder comparison, repeatability, cancellation and
  cross-text state restoration. Add listening checks for age, timbre, prosody,
  pronunciation and artifacts; WER alone is insufficient.
- State precisely which comparisons are bit-exact. Never imply bitwise identity
  with upstream PyTorch when only the optimized Core ML baseline matched.
- Change the default weights only after the quantized candidate passes the
  quality gate and improves measured latency/throughput. Otherwise publish FP16
  as the default and keep Q8 experimental.
- Validate a clean download on supported Python versions without the source
  repository, `torch`, `transformers`, gRPC or Xcode. Verify every payload using
  `SHA256SUMS` and mirror the release folder so stale or duplicate HF files are
  deleted.

Current evidence for the next card revision (M4 MacBook Air, macOS 26.5,
Core ML Tools 9.0): seven measured FP16 runs after two warmups produced identical
PCM, 69.3/76.4 ms first-PCM p50/p95 and 1.418x median real-time throughput. Thus
the throughput gate is met, while the 50 ms first-byte gate remains open. Do not
round 69.3 ms down or describe the internal PCM boundary as network/audible
latency.

The same-graph `CPU_ONLY` baseline currently crashes reproducibly at the first
explicit-history decoder prediction inside Core ML (`SIGSEGV`, exit 139). The
benchmark now refuses that unsafe path unless explicitly overridden. Publish
this as an unresolved compatibility result, not an ANE speedup, until CPU-only
completes the identical workload. Quantization copy must likewise explain that
the first Q8 talker candidate was rejected despite its size/speed because exact
code comparison and listening detected a voice-character change.

Latency work after the quality-first release:

1. Fuse the stateful talker step and fused residual predictor into one Core ML
   program so the hidden tensor stays inside one ANE invocation.
2. Export a first-subchunk decoder and prove that concatenating its output with
   the continued history decoder preserves the accepted PCM boundary.
3. Keep invariant-prefix KV reuse opt-in until both fresh-text and long-context
   streams pass exact code/PCM comparison. The current production-safe restore
   path is exact relative to sequential prefill, but its 100-trial result is
   50.8/56.5 ms p50/p95 and its full WAV differs from the batched-prefill release
   reference; it is not the HF default.
4. Re-run at least 100 isolated fresh-text trials after each change. The 50 ms
   target is achieved only when p95—not the best run or p50—is at or below the
   threshold without cached audio, silent pre-roll or transport headers counted
   as PCM.

# Bidirectional gRPC (experimental)

Run `qwen-env/bin/python serve.py --port 8766`. This adapter is maintained only
in the source repository and is deliberately excluded from the Hugging Face
model bundle.
The loopback-only `tts.v1.Speech/Synthesize` RPC accepts streamed `TextPart`
messages and returns PCM16 little-endian, mono, 24 kHz `AudioChunk` messages.
Finish text with client half-close (`done_writing()`); cancel the RPC to stop.
Incomplete words are buffered. One synthesis runs at a time; concurrent calls
receive `RESOURCE_EXHAUSTED`. Input is bounded to 32,768 characters / 4,096
parts, with a 30-second input/output idle timeout. No TLS or authentication is
provided: do not expose this diagnostic server remotely.

```sh
uv pip install --python qwen-env/bin/python -r requirements-streaming.txt
qwen-env/bin/python -m grpc_tools.protoc -I. --python_out=. --grpc_python_out=. tts_stream.proto
qwen-env/bin/python -m unittest test_voice_grpc
qwen-env/bin/python check_grpc_stream.py --reference reports/qwen06-grpc-server-01.wav
```

Verified with the real ANE runtime: first PCM arrives after sending only `I'm `,
before sending the rest of the sentence. All 51 chunks (195,840 PCM bytes) match
the same runtime's full-text WAV exactly. On the release frontend, 30 warm gRPC
trials measured client first-PCM p50/p95 at 70.6/133.7 ms. Three full 4.08-second
streams ran at 1.299x, 1.328x and 1.350x real-time. This does not meet the 30 ms
p95 target. Real-runtime stability passed short, technical, numeric, punctuation
and long inputs, repeatability, cancellation at multiple frames and cross-text
state restoration. Transport unit tests additionally cover waiting cancellation,
reuse, busy rejection and invalid input. The WebSocket prototype was removed.
