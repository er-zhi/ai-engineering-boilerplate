"""Compare a fused candidate on captured real inputs, without text-model reload.

This measures only the residual predictor, never request-to-audio latency.
"""
import argparse
import json
from pathlib import Path
import time

import coremltools as ct
import numpy as np
from voice_stream import admit_ane_package


def load(path,cache,gate,fast_prediction=False):
    admission = admit_ane_package(path,gate,cache,fast_prediction)
    hints = {'specializationStrategy':ct.SpecializationStrategy.FastPrediction} if fast_prediction else None
    return ct.models.CompiledMLModel(admission['compiled_model'],compute_units=ct.ComputeUnit.CPU_AND_NE,
        optimization_hints=hints),admission


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('candidate',type=Path)
    parser.add_argument('samples',type=Path)
    parser.add_argument('--reference',type=Path,required=True)
    parser.add_argument('--gate',type=Path,required=True)
    parser.add_argument('--cache',type=Path,required=True)
    parser.add_argument('--output',type=Path,required=True)
    parser.add_argument('--grouped',action='store_true',help='Measure each model consecutively to avoid switching ANE models each call; may introduce order/thermal bias')
    parser.add_argument('--fast-prediction',action='store_true',help='Candidate only: public Core ML specialization strategy, checked with the same gate configuration')
    args = parser.parse_args()
    if args.output.exists():
        parser.error('Output already exists')
    print('Loading candidate',flush=True)
    candidate,admission = load(args.candidate,args.cache,args.gate,args.fast_prediction)
    print('Loading reference',flush=True)
    reference,reference_admission = load(args.reference,args.cache,args.gate)
    with np.load(args.samples,allow_pickle=False) as data:
        samples = [{name:data[name][i].copy() for name in ['past_hidden','first_embedding']}
            for i in range(len(data['past_hidden']))]
    if not samples:
        raise ValueError('No captured inputs')
    errors = []
    for sample in samples:
        actual = candidate.predict(sample)['codes']
        expected = reference.predict(sample)['codes']
        if actual.shape != (1,15) or not np.isfinite(actual).all() or np.any(actual != np.floor(actual)) or np.any((actual<0)|(actual>=2048)):
            raise ValueError('Invalid candidate audio codes')
        errors.append(int(np.count_nonzero(actual != expected)))
    durations = {'candidate':[],'reference':[]}
    models = {'candidate':candidate,'reference':reference}
    schedule = ([(trial,name) for name in models for trial in range(105)] if args.grouped
        else [(trial,name) for trial in range(105) for name in
            (['candidate','reference'] if trial%2 else ['reference','candidate'])])
    for trial,name in schedule:
        started = time.perf_counter()
        models[name].predict(samples[0])
        elapsed = (time.perf_counter()-started)*1000
        if trial >= 5:
            durations[name].append(elapsed)
    report = {'scope':'Fused residual predictor only; excludes prefill, decoder, transport',
        'candidate':str(args.candidate),'reference':str(args.reference),
        'input_frames':len(samples),'different_codes_per_frame':errors,
        'different_codes':sum(errors),'total_codes':len(samples)*15,
        'quality_validated':False,'ane_admission':admission,'trials':100,'warmups':5,
        'fast_prediction':args.fast_prediction,
        'reference_ane_admission':reference_admission,
        'measurement_order':'grouped_candidate_first' if args.grouped else 'alternating',
        'timings_ms':{name:{'p50':float(np.percentile(values,50)),'p95':float(np.percentile(values,95)),
            'raw':values} for name,values in durations.items()}}
    args.output.parent.mkdir(parents=True,exist_ok=True)
    args.output.write_text(json.dumps(report,indent=2)+'\n')
    print(json.dumps({k:v for k,v in report.items() if k not in ['timings_ms','ane_admission','reference_ane_admission','different_codes_per_frame']}),flush=True)
    print(json.dumps({name:{k:v for k,v in row.items() if k!='raw'} for name,row in report['timings_ms'].items()}),flush=True)


if __name__ == '__main__':
    main()
