"""Benchmark one release pipeline on CPU-only or CPU+ANE Core ML compute units."""

import argparse
import hashlib
import json
import resource
import statistics
import time
from pathlib import Path

import coremltools as ct
from voice_stream import VoiceStream


def percentile(values, fraction):
    ordered = sorted(values)
    position = (len(ordered) - 1) * fraction
    lower = int(position)
    upper = min(lower + 1, len(ordered) - 1)
    return ordered[lower] + (ordered[upper] - ordered[lower]) * (position - lower)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--compute", choices=("cpu", "ane"), required=True)
    parser.add_argument(
        "--allow-unsafe-cpu-only",
        action="store_true",
        help=(
            "Allow the CPU_ONLY decoder path. It reproducibly segfaults inside "
            "Core ML on the currently tested macOS 26.5/Core ML Tools 9.0 stack."
        ),
    )
    parser.add_argument("--gate", type=Path)
    parser.add_argument(
        "--prefix-kv",
        action="store_true",
        help="Experimental invariant-prefix KV mode; report it separately from the release baseline",
    )
    parser.add_argument("--trials", type=int, default=5)
    parser.add_argument("--warmups", type=int, default=2)
    parser.add_argument("--text", default="I'm sorry about the charge. I'll fix it for you.")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if args.trials < 1 or args.warmups < 0:
        parser.error("trials must be positive and warmups non-negative")
    if args.compute == "ane" and (args.gate is None or not args.gate.is_file()):
        parser.error("--gate is required for strict ANE benchmarking")
    if args.compute == "cpu" and not args.allow_unsafe_cpu_only:
        parser.error(
            "CPU_ONLY is disabled because the current explicit-history decoder "
            "reproducibly exits with SIGSEGV. Use --allow-unsafe-cpu-only only "
            "in an isolated process while investigating the Core ML runtime bug."
        )
    root = Path(__file__).resolve().parent
    models = root / "models"
    units = ct.ComputeUnit.CPU_ONLY if args.compute == "cpu" else ct.ComputeUnit.CPU_AND_NE
    voice = VoiceStream(
        models / "qwen06-runtime-assets-hybrid",
        models / "qwen06-stateful-fp16",
        args.gate if args.compute == "ane" else None,
        args.text,
        "",
        compiled_dir=models / "compiled-cache",
        predictor_package=models / "qwen06-outlier256-w8-safe-down.mlpackage",
        prefill_packages=None if args.prefix_kv else models / "qwen06-full-fp16",
        speaker="Serena",
        model_prefix="qwen06",
        block_count=1,
        decoder_package=models / "streaming-decoder-explicit-noslice-fp16.mlpackage",
        experimental_history_decoder=True,
        experimental_prefix_state=args.prefix_kv,
        text_projection_package=models / "qwen06-text-projection32.mlpackage",
        long_talker_package=models / "qwen06-long512-fp16.mlpackage",
        frontend_assets=models / "qwen06-runtime-assets-hybrid",
        compute_units=units,
    )
    rows = []
    for trial in range(args.warmups + args.trials):
        started = time.perf_counter()
        first = None
        payload = bytearray()
        for pcm, _ in voice.chunks_for_text(args.text, 502):
            if first is None:
                first = (time.perf_counter() - started) * 1000
            payload.extend(pcm)
        elapsed = time.perf_counter() - started
        rows.append(
            {
                "warmup": trial < args.warmups,
                "first_pcm_ms": first,
                "elapsed_s": elapsed,
                "audio_s": len(payload) / 48000,
                "realtime_multiple": (len(payload) / 48000) / elapsed,
                "pcm_bytes": len(payload),
                "pcm_sha256": hashlib.sha256(payload).hexdigest(),
            }
        )
    measured = [row for row in rows if not row["warmup"]]
    report = {
        "compute": args.compute,
        "hardware": __import__("platform").platform(),
        "text": args.text,
        "warmups": args.warmups,
        "trials": args.trials,
        "first_pcm_ms": {
            "p50": statistics.median(x["first_pcm_ms"] for x in measured),
            "p95": percentile([x["first_pcm_ms"] for x in measured], 0.95),
        },
        "realtime_multiple": {
            "p50": statistics.median(x["realtime_multiple"] for x in measured),
            "minimum": min(x["realtime_multiple"] for x in measured),
        },
        "deterministic_pcm": len({x["pcm_sha256"] for x in measured}) == 1,
        "peak_rss_bytes": resource.getrusage(resource.RUSAGE_SELF).ru_maxrss,
        "rows": rows,
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2), flush=True)


if __name__ == "__main__":
    main()
