"""Stream the downloaded HF model into a PCM WAV file."""

import argparse
import importlib
import sys
import time
import wave
from pathlib import Path

ROOT = Path(__file__).resolve().parent


def load_api(model_dir):
    model_dir = Path(model_dir).resolve()
    if not (model_dir / "qwen3_tts_ane.py").is_file():
        raise RuntimeError(
            f"Model is not installed at {model_dir}; run download_model.py first"
        )
    sys.path.insert(0, str(model_dir))
    return importlib.import_module("qwen3_tts_ane").Qwen3TTSANE


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("text")
    parser.add_argument("--model", type=Path, default=ROOT / "model")
    parser.add_argument("--output", type=Path, default=Path("output.wav"))
    parser.add_argument("--max-frames", type=int, default=502)
    parser.add_argument("--prefix-kv", action="store_true")
    args = parser.parse_args()

    api = load_api(args.model)
    voice = api(root=args.model, use_prefix_kv=args.prefix_kv)
    started = time.perf_counter()
    chunks = 0
    with wave.open(str(args.output), "wb") as output:
        output.setnchannels(1)
        output.setsampwidth(2)
        output.setframerate(24000)
        for chunk in voice.stream(args.text, args.max_frames):
            if chunks == 0:
                elapsed_ms = (time.perf_counter() - started) * 1000
                print(f"First PCM: {elapsed_ms:.1f} ms", flush=True)
            output.writeframesraw(chunk.pcm_s16le)
            chunks += 1
    print(f"{chunks * 0.08:.2f} seconds -> {args.output}", flush=True)


if __name__ == "__main__":
    main()
