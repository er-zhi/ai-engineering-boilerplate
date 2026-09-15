"""Create a separate per-channel INT8-weight candidate; never overwrite FP16."""

import argparse
import re
import shutil
from pathlib import Path

import coremltools as ct
from coremltools.optimize.coreml import (
    OpLinearQuantizerConfig,
    OptimizationConfig,
    linear_quantize_weights,
)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument(
        "--matrices-only",
        action="store_true",
        help="Quantize linear/conv weights, retain normalization constants",
    )
    parser.add_argument(
        "--granularity",
        choices=("per_channel", "per_block"),
        default="per_channel",
        help="INT8 scale granularity; per_block follows Q8_0-style local scaling",
    )
    parser.add_argument(
        "--mode",
        choices=("linear_symmetric", "linear"),
        default="linear_symmetric",
        help="Symmetric INT8 or asymmetric INT8 with a learned zero point",
    )
    parser.add_argument(
        "--block-size",
        type=int,
        default=32,
        help="Input-axis block size used with --granularity per_block",
    )
    parser.add_argument(
        "--keep-predictor-heads",
        action="store_true",
        help="Require and preserve all 15 fused predictor output-head weights",
    )
    parser.add_argument(
        "--keep-predictor-embeddings",
        action="store_true",
        help="Preserve 14 folded embedding-selection weights in the fused15 graph",
    )
    parser.add_argument(
        "--keep-weight",
        action="append",
        default=[],
        help="Exact constant weight name to retain; repeat for multiple matrices",
    )
    parser.add_argument(
        "--keep-layer-range",
        action="append",
        default=[],
        metavar="START:END",
        help="Keep transformer layers in the half-open range [START, END) at FP16",
    )
    args = parser.parse_args()
    if args.output.exists():
        parser.error("output already exists")
    model = ct.models.MLModel(str(args.source), skip_model_load=True)
    if args.block_size <= 0:
        parser.error("--block-size must be positive")
    op = OpLinearQuantizerConfig(
        mode=args.mode,
        dtype="int8",
        granularity=args.granularity,
        block_size=args.block_size,
    )
    config = (
        OptimizationConfig(op_type_configs={name: op for name in ["linear", "conv", "matmul"]})
        if args.matrices_only
        else OptimizationConfig(global_config=op)
    )
    protected = set()
    if (
        args.keep_predictor_heads
        or args.keep_predictor_embeddings
        or args.keep_weight
        or args.keep_layer_range
    ):
        # Official OptimizationConfig: named constants set to None are excluded.
        # https://apple.github.io/coremltools/source/coremltools.optimize.coreml.utilities.html
        spec = model.get_spec()
        block = next(iter(spec.mlProgram.functions["main"].block_specializations.values()))
        constants = {
            output.name
            for operation in block.operations
            if operation.type == "const"
            for output in operation.outputs
        }
        if any(name not in constants for name in args.keep_weight):
            raise ValueError("Requested protected weight is not a source constant")
        protected.update(args.keep_weight)
        for value in args.keep_layer_range:
            match = re.fullmatch(r"(\d+):(\d+)", value)
            if match is None or int(match.group(1)) >= int(match.group(2)):
                parser.error("--keep-layer-range must be START:END with START < END")
            start, end = map(int, match.groups())
            selected = {
                name
                for name in constants
                if (layer := re.search(r"(?:^|_)model_layers_(\d+)_", name))
                and start <= int(layer.group(1)) < end
            }
            if not selected:
                raise ValueError(f"No weights found for transformer layers {value}")
            protected.update(selected)
        heads = {
            operation.inputs["weight"].arguments[0].name
            for operation in block.operations
            if operation.type == "linear"
            and operation.inputs["weight"].arguments[0].name.startswith("heads_")
        }
        if args.keep_predictor_heads and len(heads) != 15:
            raise ValueError(f"Expected 15 predictor head constants, found {len(heads)}")
        if args.keep_predictor_heads:
            protected.update(heads)
        if args.keep_predictor_embeddings:
            embeddings = {
                operation.inputs["weight"].arguments[0].name
                for operation in block.operations
                if operation.type == "linear"
                and operation.inputs["weight"].arguments[0].name.startswith("transpose_")
            }
            if len(embeddings) != 14:
                raise ValueError(f"Expected 14 folded embedding weights, found {len(embeddings)}")
            protected.update(embeddings)
        for name in protected:
            config.set_op_name(name, None)
    result = linear_quantize_weights(model, config=config)
    if protected:
        block = next(
            iter(result.get_spec().mlProgram.functions["main"].block_specializations.values())
        )
        producers = {
            output.name: operation.type
            for operation in block.operations
            for output in operation.outputs
        }
        if any(producers.get(name) != "const" for name in protected):
            raise RuntimeError("Protected weight preservation failed")
    args.output.parent.mkdir(parents=True, exist_ok=True)
    result.save(str(args.output))
    fixture = args.source.with_suffix(".inputs.npz")
    if fixture.exists():
        shutil.copy2(fixture, args.output.with_suffix(".inputs.npz"))
    print(f"Saved {args.output}", flush=True)


if __name__ == "__main__":
    main()
