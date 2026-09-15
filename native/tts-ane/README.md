# Qwen3-TTS ANE consumer

This directory is intentionally only a thin integration layer. The ready-made
Core ML model is downloaded from
[`erjigit17/Qwen3-TTS-0.6B-ANE`](https://huggingface.co/erjigit17/Qwen3-TTS-0.6B-ANE)
at the immutable revision in `MODEL_SOURCE.json`.

The conversion code, benchmarks, verification tools and reusable ANE engineering
knowledge live in
[`er-zhi/qwen3-tts-ane`](https://github.com/er-zhi/qwen3-tts-ane). Do not add
model conversion experiments or weights back to this boilerplate.

## Install and run

Apple Silicon macOS and Python 3.10–3.13 are required.

```bash
python3 -m venv .venv-tts
. .venv-tts/bin/activate
pip install -r native/tts-ane/requirements.txt
python native/tts-ane/download_model.py
pip install -r native/tts-ane/model/requirements.txt
python native/tts-ane/model/verify_install.py --checksums
python native/tts-ane/run.py \
  "I'm sorry about the charge. I'll fix it for you." \
  --output answer.wav
```

The download is pinned for reproducibility. Upgrade it deliberately by updating
`MODEL_SOURCE.json` after validating the new HF revision. `model/`, generated
WAV files and Core ML caches remain local and are not committed.

`run.py` is a local smoke-test adapter. Production transports such as gRPC belong
to the consuming service; the model artifact and its source repository remain
transport-neutral.
