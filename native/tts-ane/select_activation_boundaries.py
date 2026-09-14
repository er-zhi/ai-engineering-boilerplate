"""Export a separate diagnostic excluding linear-output Q/DQ pairs.

This changes quantization numerics, not layers or codebook count. Neither ANE
placement, INT8 kernel execution nor quality is implied by successful export.
"""
import argparse
from pathlib import Path

import coremltools as ct


def exclude_linear_outputs(block):
    producers = {v.name: op for op in block.operations for v in op.outputs}
    quantizers = {op.outputs[0].name: op for op in block.operations
                  if op.type == 'quantize'}
    excluded = {name for name, op in quantizers.items()
                if producers[op.inputs['input'].arguments[0].name].type == 'linear'}
    replacements, removed = {}, set()
    for op in block.operations:
        if op.type != 'dequantize':
            continue
        name = op.inputs['input'].arguments[0].name
        if name in excluded:
            replacements[op.outputs[0].name] = quantizers[name].inputs['input'].arguments[0].name
            removed.update((name, op.outputs[0].name))
    if any(name in removed for name in block.outputs):
        raise ValueError('Diagnostic does not rewrite public quantized outputs')
    retained = []
    for op in block.operations:
        if any(v.name in removed for v in op.outputs):
            continue
        for value in op.inputs.values():
            for argument in value.arguments:
                seen = set()
                while argument.name in replacements:
                    if argument.name in seen:
                        raise ValueError('Cyclic quantization dependency')
                    seen.add(argument.name)
                    argument.name = replacements[argument.name]
                if argument.name in removed:
                    raise ValueError('Quantized output has another consumer')
        retained.append(op)
    del block.operations[:]
    block.operations.extend(retained)
    return len(quantizers) - len(excluded), len(excluded)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('source', type=Path)
    parser.add_argument('output', type=Path)
    args = parser.parse_args()
    if args.output.exists():
        parser.error('Never overwrite an existing model')
    model = ct.models.MLModel(str(args.source), skip_model_load=True)
    spec = model.get_spec()
    blocks = spec.mlProgram.functions['main'].block_specializations
    if len(blocks) != 1:
        parser.error('Expected a single specialization')
    retained, excluded = exclude_linear_outputs(next(iter(blocks.values())))
    ct.models.MLModel(spec, weights_dir=model.weights_dir,
                      skip_model_load=True).save(str(args.output))
    print(f'Saved diagnostic: {retained} pairs retained; {excluded} excluded')


if __name__ == '__main__':
    main()
