"""Paired populated-KV transfer experiment on identical prefill inputs."""
import argparse
import json
from pathlib import Path
import time
import numpy as np
from benchmark_fused_predictor import load


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('reference',type=Path)
    parser.add_argument('candidate',type=Path)
    parser.add_argument('--gate',type=Path,required=True)
    parser.add_argument('--output',type=Path,required=True)
    args = parser.parse_args()
    if args.output.exists():
        parser.error('Preserve previous evidence')
    with np.load(args.candidate.with_suffix('.inputs.npz'),allow_pickle=False) as fixture:
        inputs = {name:fixture[name].copy() for name in ['embeddings','cosine','sine','attention_mask']}
    models = {name:load(path,Path('models/compiled-cache'),args.gate)[0]
        for name,path in [('reference',args.reference),('candidate',args.candidate)]}
    outputs = {name:model.predict(inputs) for name,model in models.items()}
    errors = {}
    for name,actual in outputs['candidate'].items():
        expected = outputs['reference'][name]
        if name in ['next_keys','next_values']:
            expected = expected[:,:,:actual.shape[2]]
        errors[name] = float(np.max(np.abs(actual-expected)))
        if actual.shape != expected.shape or not np.isfinite(actual).all():
            raise ValueError('Invalid prefill output: '+name)
    times = {name:[] for name in models}
    for trial in range(105):
        for name in (['reference','candidate'] if trial%2 else ['candidate','reference']):
            start = time.perf_counter()
            output = models[name].predict(inputs)
            # Match runtime safety checks, including the returned KV arrays.
            if not all(np.isfinite(value).all() for value in output.values()):
                raise ValueError('Non-finite measured output')
            if trial >= 5:
                times[name].append((time.perf_counter()-start)*1000)
    report = {'scope':'Prefill plus output finite checks only; deterministic synthetic inputs, not TTFB',
        'reference':str(args.reference),'candidate':str(args.candidate),
        'populated_output_peak_error':errors,'populated_outputs_exact':all(value==0 for value in errors.values()),
        'trials':100,'order':'alternating','timings_ms':{name:{'p50':float(np.percentile(values,50)),
            'p95':float(np.percentile(values,95)),'raw':values} for name,values in times.items()},
        'median_paired_saving_ms':float(np.median(np.array(times['reference'])-times['candidate']))}
    args.output.write_text(json.dumps(report,indent=2)+'\n')
    print(json.dumps({k:v for k,v in report.items() if k!='timings_ms'}),flush=True)
    print(json.dumps({name:{k:v for k,v in row.items() if k!='raw'} for name,row in report['timings_ms'].items()}),flush=True)


if __name__ == '__main__':
    main()
