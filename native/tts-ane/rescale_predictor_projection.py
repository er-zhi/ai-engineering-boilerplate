"""Diagnostic equivalent linear scaling; no latency/quality claim at export."""
import argparse
from pathlib import Path

import coremltools as ct
import numpy as np
from coremltools.converters.mil.frontend.milproto.load import load
from coremltools.converters.mil.mil import Builder as mb


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('source', type=Path)
    parser.add_argument('output', type=Path)
    parser.add_argument('--weight', default='core_model_layers_2_mlp_down_proj_weight_to_fp16')
    parser.add_argument('--expected-calls', type=int, default=15)
    parser.add_argument('--allow-subnormal-rounding', action='store_true', help='Diagnostic only: record FP16 constant-rounding error instead of claiming exactness')
    args = parser.parse_args()
    if args.output.exists() or args.expected_calls < 1:
        parser.error('Require new output and positive expected call count')
    model = ct.models.MLModel(str(args.source), skip_model_load=True)
    spec = model.get_spec()
    program = load(spec, spec.specificationVersion, file_weights_dir=model.weights_dir)
    function = program.functions['main']
    selected = [op for op in function.operations if op.op_type == 'linear'
                and op.weight.name == args.weight]
    if len(selected) != args.expected_calls:
        raise ValueError(f'Expected {args.expected_calls} calls; found {len(selected)}')
    for op in selected:
        if op.weight.val is None or op.bias.val is None:
            raise ValueError('Requires materialized protected weights and bias')
        weight, bias = np.asarray(op.weight.val), np.asarray(op.bias.val)
        if not np.isfinite(weight).all() or not np.isfinite(bias).all():
            raise ValueError('Nonfinite source constants')
        # Reject values lost by the power-of-two scaling itself.
        for value in [weight, bias]:
            reconstructed = (value / np.float16(4))*np.float16(4)
            if not np.array_equal(reconstructed, value):
                if not args.allow_subnormal_rounding:
                    raise ValueError('Scaling loses constant precision; needs another representation')
                error = np.abs(reconstructed.astype(np.float32)-value.astype(np.float32))
                print(f'{op.name}: rounded constants={np.count_nonzero(error)}, max error={error.max()}', flush=True)
        with function:
            partial = mb.linear(x=op.x, weight=weight/np.float16(4),
                bias=bias/np.float16(4), name=op.name+'_scaled', before_op=op)
            restored = mb.mul(x=partial, y=np.float16(4), name=op.name+'_restored', before_op=op)
            function.replace_uses_of_var_after_op(anchor_op=op, old_var=op.outputs[0], new_var=restored)
            function.remove_ops([op])
    result = ct.convert(program, convert_to='mlprogram', minimum_deployment_target=ct.target.macOS15,
        pass_pipeline=ct.PassPipeline.EMPTY, skip_model_load=True)
    result.save(str(args.output))
    print(f'Saved {args.output}; rescaled calls: {len(selected)}', flush=True)


if __name__ == '__main__':
    main()
