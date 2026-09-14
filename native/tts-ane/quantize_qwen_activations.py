"""Experimental Core ML activation calibration from captured real inputs.

Uses coremltools 9 linear_quantize_activations. Calibration is CPU-only to
avoid repeatedly compiling diagnostic intermediate-output graphs for ANE.
The resulting candidate still requires separate ANE admission and audio tests.
"""
import argparse
import json
from pathlib import Path
import time
import hashlib
from collections import defaultdict

import coremltools as ct
import numpy as np
from coremltools.optimize.coreml import (
    OptimizationConfig, OpLinearQuantizerConfig, linear_quantize_activations,
)


def update_range(stats,name,value):
    low,high = float(np.min(value)),float(np.max(value))
    previous = stats.get(name)
    stats[name] = {'rmin':min(low,previous['rmin']) if previous else low,
        'rmax':max(high,previous['rmax']) if previous else high}


def fp32_calibration_range(stats,var_name):
    """Preserve FP32 ranges for a wholly FP32 diagnostic graph.

    coremltools 9.0 hardcodes float16 in this helper, overflowing valid ranges
    and supplying the wrong scale dtype in the suffix activation pass. There
    is no dtype option in that API. Override only within this export process.
    Source: https://github.com/apple/coremltools/blob/9.0/coremltools/optimize/_utils.py
    """
    values = np.asarray([stats[var_name]['rmin'],stats[var_name]['rmax']],dtype=np.float32)
    if not np.isfinite(values).all() or values[0] > values[1]:
        raise ValueError(f'Invalid activation range for {var_name}')
    return values


def reusable_calibration(model,samples,group_size):
    """Pinned 9.0 debugger protocol, one compiled graph per output group.

    Unlike 9.0's supplied collector, preserve extrema across every sample.
    No site-packages are modified; the override is scoped to this process.
    """
    from coremltools.optimize.coreml.experimental._model_debugger import ModelDebugger, ModelInfo
    from coremltools.optimize.coreml.experimental._post_training_quantization import _adjust_concat_surrounding_activation_stats
    if ct.__version__ != '9.0':
        raise RuntimeError('Recheck debugger compatibility before changing coremltools version')
    debugger = ModelDebugger(model)
    outputs = {output.name for output in model.get_spec().description.output}
    names = sorted(set(debugger.get_intermediate_output_names(lambda op:op.spec.type != 'const'))-outputs)
    stats = defaultdict(dict)
    for sample in samples:
        for name,value in sample.items():
            update_range(stats,name,value)
    groups = list(ModelDebugger.batch(names,group_size))
    for index,names in enumerate(groups):
        started = time.perf_counter()
        spec = ModelDebugger.clone_spec(debugger.model_info.spec)
        info = ModelInfo(ModelDebugger.get_program_info(spec.mlProgram),spec)
        block = ModelDebugger.get_any_block(info)
        selected = []
        for name in names:
            dtype = ModelDebugger.get_output_feature_type(name,debugger.block_info.operations)
            if dtype is None:
                continue
            block.spec.outputs.append(name)
            output = spec.description.output.add()
            output.name = name
            output.type.multiArrayType.dataType = dtype
            selected.append(name)
        probe = ct.models.MLModel(spec,weights_dir=debugger.weights_dir,compute_units=ct.ComputeUnit.CPU_ONLY)
        for sample in samples:
            result = probe.predict(sample)
            for name in selected:
                update_range(stats,name,result[name])
        del probe
        print(json.dumps({'calibration_group':index+1,'groups':len(groups),
            'samples':len(samples),'elapsed_s':time.perf_counter()-started}),flush=True)
    _adjust_concat_surrounding_activation_stats(debugger._get_concat_op_info(),stats)
    return stats


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('source',type=Path)
    parser.add_argument('samples',type=Path)
    parser.add_argument('--output',type=Path,required=True)
    parser.add_argument('--sample-count',type=int,default=8)
    parser.add_argument('--group-size',type=int,default=512)
    parser.add_argument('--fp32-calibration-ranges',action='store_true',help='Scoped coremltools 9.0 dtype correction, only for an entirely FP32 graph')
    args = parser.parse_args()
    if args.output.exists() or not 1 <= args.sample_count <= 128 or not 1 <= args.group_size <= 1024:
        parser.error('Require new output, 1..128 samples, and 1..1024 intermediate outputs per group')
    with np.load(args.samples,allow_pickle=False) as archive:
        names = ['past_hidden','first_embedding']
        count = len(archive[names[0]])
        if not count or any(len(archive[name]) != count for name in names):
            parser.error('Empty or inconsistent calibration data')
        indices = np.unique(np.linspace(0,count-1,min(count,args.sample_count),dtype=int))
        samples = [{name:archive[name][index].copy() for name in names} for index in indices]
    if any(not np.isfinite(value).all() for sample in samples for value in sample.values()):
        parser.error('Non-finite calibration inputs')
    started = time.perf_counter()
    model = ct.models.MLModel(str(args.source),compute_units=ct.ComputeUnit.CPU_ONLY)
    if args.fp32_calibration_ranges:
        from coremltools.proto.MIL_pb2 import FLOAT16
        if ct.__version__ != '9.0':
            raise RuntimeError('Recheck the upstream range helper before using this correction')
        def has_fp16(block):
            return any(output.type.tensorType.dataType == FLOAT16 for op in block.operations for output in op.outputs) or any(
                has_fp16(nested) for op in block.operations for nested in op.blocks)
        if any(has_fp16(block) for function in model.get_spec().mlProgram.functions.values()
            for block in function.block_specializations.values()):
            raise ValueError('FP32 range correction cannot be applied to mixed/FP16 graphs')
    for sample in samples:
        codes = model.predict(sample)['codes']
        if not np.isfinite(codes).all() or np.any(codes != np.floor(codes)) or np.any((codes<0)|(codes>=2048)):
            raise RuntimeError('CPU calibration path returns invalid audio codes; its activation ranges must not be used')
    # Keep masking, residual adds, selection and normalization in FP16. Zero
    # ranges in those auxiliary operations cannot define an INT8 scale.
    config = OptimizationConfig(op_type_configs={'linear':OpLinearQuantizerConfig(mode='linear_symmetric',dtype='int8')})
    digest = hashlib.sha256(f'{ct.__version__}:collector-v1:{indices.tolist()}'.encode())
    for path in [args.samples,*sorted(p for p in args.source.rglob('*') if p.is_file())]:
        with path.open('rb') as stream:
            digest.update(hashlib.file_digest(stream,'sha256').digest())
    stats_path = args.output.with_suffix(f'.stats-{digest.hexdigest()[:20]}.json')
    def collect(model,data,group_size):
        if stats_path.exists():
            print('Reusing measured activation ranges, not audio',flush=True)
            return {name:{key:float(value) for key,value in limits.items()}
                for name,limits in json.loads(stats_path.read_text()).items()}
        stats = reusable_calibration(model,data,group_size)
        stats_path.parent.mkdir(parents=True,exist_ok=True)
        stats_path.write_text(json.dumps({name:{key:str(value) for key,value in limits.items()}
            for name,limits in stats.items()},indent=2)+'\n')
        return stats
    print(f'Calibrating {len(samples)} real frames, {args.group_size} intermediate outputs per group',flush=True)
    from coremltools.optimize.coreml import _post_training_quantization as implementation
    from coremltools.optimize import _utils as optimization_utils
    original = implementation._get_activation_calibration_stats
    original_range = optimization_utils.get_min_and_max_values
    try:
        implementation._get_activation_calibration_stats = collect
        if args.fp32_calibration_ranges:
            optimization_utils.get_min_and_max_values = fp32_calibration_range
        result = linear_quantize_activations(model,config,samples,calibration_op_group_size=args.group_size)
    finally:
        implementation._get_activation_calibration_stats = original
        optimization_utils.get_min_and_max_values = original_range
    args.output.parent.mkdir(parents=True,exist_ok=True)
    result.save(args.output)
    report = {'status':'EXPORTED_NOT_VALIDATED','source':str(args.source),'samples':str(args.samples),
        'selected_indices':indices.tolist(),'group_size':args.group_size,
        'collector':'reused output-group models; extrema accumulated across all selected samples',
        'calibration_compute_units':'CPU_ONLY','elapsed_s':time.perf_counter()-started,
        'production_calibration':False,'quality_validated':False,'ane_validated':False}
    report['fp32_calibration_ranges'] = args.fp32_calibration_ranges
    args.output.with_suffix('.calibration.json').write_text(json.dumps(report,indent=2)+'\n')
    print(json.dumps(report),flush=True)


if __name__ == '__main__':
    main()
