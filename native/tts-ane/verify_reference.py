"""Regenerate the packaged reference prompt and require byte-exact PCM."""

import argparse
import hashlib
import json
import wave
from pathlib import Path

from qwen3_tts_ane import Qwen3TTSANE

REFERENCE_TEXT = "I'm sorry about the charge. I'll fix it for you."


def read_reference(path):
    with wave.open(str(path), "rb") as source:
        if source.getparams()[:3] != (1, 2, 24000):
            raise RuntimeError("Reference WAV must be mono 24 kHz PCM16")
        return source.readframes(source.getnframes())


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--runs", type=int, default=1)
    args = parser.parse_args()
    if not 1 <= args.runs <= 100:
        parser.error("--runs must be 1..100")
    root = Path(__file__).resolve().parent
    expected = read_reference(root / "samples" / "serena.wav")
    expected_hash = hashlib.sha256(expected).hexdigest()
    voice = Qwen3TTSANE(root=root)
    results = []
    for run in range(args.runs):
        chunks = list(voice.stream(REFERENCE_TEXT))
        pcm = b"".join(chunk.pcm_s16le for chunk in chunks)
        digest = hashlib.sha256(pcm).hexdigest()
        results.append(
            {
                "run": run + 1,
                "chunks": len(chunks),
                "pcm_bytes": len(pcm),
                "first_pcm_ms": chunks[0].metadata["ready_ms"],
                "pcm_sha256": digest,
                "exact": pcm == expected,
            }
        )
    passed = all(item["exact"] for item in results)
    print(
        json.dumps(
            {
                "status": "PASS" if passed else "FAIL",
                "reference_text": REFERENCE_TEXT,
                "reference_pcm_sha256": expected_hash,
                "runs": results,
            },
            indent=2,
        )
    )
    if not passed:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
