"""Writes streamed Qwen3-TTS ANE output to a PCM WAV file."""

import argparse
import time
import wave
from pathlib import Path

from qwen3_tts_ane import Qwen3TTSANE


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("text")
    parser.add_argument("--output", type=Path, default=Path("output.wav"))
    parser.add_argument("--max-frames", type=int, default=502)
    parser.add_argument("--cache", type=Path)
    parser.add_argument(
        "--prefix-kv",
        action="store_true",
        help="Reuse the invariant Serena prefix state; generated audio is never cached",
    )
    args = parser.parse_args()
    voice = Qwen3TTSANE(cache=args.cache, use_prefix_kv=args.prefix_kv)
    started = time.perf_counter()
    chunks = 0
    with wave.open(str(args.output), "wb") as output:
        output.setnchannels(1)
        output.setsampwidth(2)
        output.setframerate(24000)
        for chunk in voice.stream(args.text, args.max_frames):
            if chunks == 0:
                print(f"First PCM: {(time.perf_counter() - started) * 1000:.1f} ms", flush=True)
            output.writeframesraw(chunk.pcm_s16le)
            chunks += 1
    print(f"{chunks * 0.08:.2f} seconds -> {args.output}", flush=True)


if __name__ == "__main__":
    main()
