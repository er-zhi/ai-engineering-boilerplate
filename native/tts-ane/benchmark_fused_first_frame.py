"""Validate and benchmark a fused no-KV first-frame Core ML candidate."""

import argparse
import json
import time
from pathlib import Path

import coremltools as ct
import numpy as np
from voice_stream import admit_ane_package


def summary(values):
    return {
        "p50": float(np.percentile(values, 50)),
        "p95": float(np.percentile(values, 95)),
        "raw": values,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("candidate", type=Path)
    parser.add_argument("inputs", type=Path)
    parser.add_argument("--gate", type=Path, required=True)
    parser.add_argument("--cache", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--trials", type=int, default=100)
    parser.add_argument("--warmups", type=int, default=5)
    args = parser.parse_args()
    if args.output.exists():
        parser.error("output already exists")
    if args.trials < 1 or args.warmups < 0:
        parser.error("trials must be positive and warmups non-negative")
    admission = admit_ane_package(args.candidate, args.gate, args.cache)
    model = ct.models.CompiledMLModel(
        admission["compiled_model"], compute_units=ct.ComputeUnit.CPU_AND_NE
    )
    with np.load(args.inputs, allow_pickle=False) as saved:
        sample = {
            name: saved[name].copy() for name in ("embeddings", "cosine", "sine", "attention_mask")
        }
        expected = {
            name: saved[name].copy()
            for name in (
                "expected_codes",
                "expected_hidden",
                "expected_keys",
                "expected_values",
            )
        }
    actual = model.predict(sample)
    codes = actual["codes"]
    if codes.shape != expected["expected_codes"].shape or not np.isfinite(codes).all():
        raise RuntimeError("Invalid fused first-frame code output")
    exact_codes = np.array_equal(codes.astype(np.int64), expected["expected_codes"])
    errors = {
        "hidden": float(np.max(np.abs(actual["hidden"] - expected["expected_hidden"]))),
        "keys": float(np.max(np.abs(actual["next_keys"] - expected["expected_keys"]))),
        "values": float(np.max(np.abs(actual["next_values"] - expected["expected_values"]))),
    }
    timings = []
    for trial in range(args.warmups + args.trials):
        started = time.perf_counter()
        result = model.predict(sample)
        elapsed = (time.perf_counter() - started) * 1000
        if not np.isfinite(result["codes"]).all():
            raise RuntimeError(f"Non-finite result at trial {trial}")
        if trial >= args.warmups:
            timings.append(elapsed)
    report = {
        "scope": (
            "No-KV first-frame talker prefill plus 16 audio codes; excludes decoder, "
            "text preparation and transport"
        ),
        "candidate": str(args.candidate.resolve()),
        "exact_fp32_codes": exact_codes,
        "codes": codes.astype(np.int64).tolist(),
        "max_abs_error": errors,
        "trials": args.trials,
        "warmups": args.warmups,
        "timings_ms": summary(timings),
        "ane_admission": admission,
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(
        json.dumps(
            {
                **{key: value for key, value in report.items() if key != "ane_admission"},
                "timings_ms": {
                    key: value for key, value in report["timings_ms"].items() if key != "raw"
                },
                "ane_status": admission["status"],
            },
            indent=2,
        ),
        flush=True,
    )
    if not exact_codes:
        raise RuntimeError("Core ML first-frame codes differ from the FP32 reference")


if __name__ == "__main__":
    main()
