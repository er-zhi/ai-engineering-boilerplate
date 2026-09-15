"""Create an ANE-oriented 8-bit k-means palettized talker candidate."""

import argparse
from pathlib import Path

import coremltools as ct
from coremltools.optimize.coreml import (
    OpPalettizerConfig,
    OptimizationConfig,
    palettize_weights,
)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--group-size", type=int, default=32)
    parser.add_argument("--workers", type=int, default=8)
    parser.add_argument("--mode", choices=("kmeans", "uniform"), default="kmeans")
    args = parser.parse_args()
    if args.output.exists():
        parser.error("output already exists")
    if args.group_size <= 0 or args.workers <= 0:
        parser.error("group size and workers must be positive")
    model = ct.models.MLModel(str(args.source), skip_model_load=True)
    op = OpPalettizerConfig(
        mode=args.mode,
        nbits=8,
        granularity="per_grouped_channel",
        group_size=args.group_size,
        enable_per_channel_scale=True,
        num_kmeans_workers=args.workers,
    )
    config = OptimizationConfig(op_type_configs={name: op for name in ("linear", "conv", "matmul")})
    result = palettize_weights(model, config=config)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    result.save(str(args.output))
    print(f"Saved {args.output}", flush=True)


if __name__ == "__main__":
    main()
