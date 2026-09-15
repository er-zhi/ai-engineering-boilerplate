"""Ready-to-run Serena English gRPC server for the packaged ANE release."""

import argparse
import asyncio
import shutil
import subprocess
from pathlib import Path

from qwen3_tts_ane import Qwen3TTSANE
from voice_grpc import serve_grpc


def build_gate(root, cache):
    gate = cache / "ane_gate"
    source = root / "ane_gate.swift"
    if not gate.exists() or gate.stat().st_mtime < source.stat().st_mtime:
        cache.mkdir(parents=True, exist_ok=True)
        xcrun = shutil.which("xcrun")
        if xcrun is None:
            raise RuntimeError("xcrun is required to build the local ANE admission gate")
        subprocess.run(  # noqa: S603 -- executable is resolved; arguments are local paths.
            [xcrun, "swiftc", "-parse-as-library", str(source), "-O", "-o", str(gate)],
            check=True,
        )
    return gate


async def run(args, root, cache):
    runtime = Qwen3TTSANE(cache=cache, gate=build_gate(root, cache))
    print(f"Shared talker weights: {runtime.shared_weight_modes}", flush=True)
    await serve_grpc(runtime._voice, args.port, args.max_frames)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--port", type=int, default=8766)
    parser.add_argument(
        "--max-frames", type=int, default=502, choices=range(1, 503), metavar="1..502"
    )
    parser.add_argument("--cache", type=Path, default=Path("~/Library/Caches/Qwen3TTSANE"))
    args = parser.parse_args()
    if not 1 <= args.port <= 65535:
        parser.error("port must be 1..65535")
    root = Path(__file__).resolve().parent
    cache = args.cache.expanduser().resolve()
    asyncio.run(run(args, root, cache))


if __name__ == "__main__":
    main()
