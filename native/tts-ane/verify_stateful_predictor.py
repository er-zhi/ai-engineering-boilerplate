"""Check stateful vs explicit-cache Core ML execution, including fresh sessions."""
import argparse
import json
import time
from pathlib import Path

import coremltools as ct
import numpy as np


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('candidate',type=Path,help='ANE-admitted compiled candidate')
    parser.add_argument('baseline',type=Path,help='Matching FP16 compiled explicit-cache predictor')
    parser.add_argument('--report',type=Path,required=True)
    parser.add_argument('--baseline-package',type=Path,default=Path('native/tts-ane/models/coreml/qwen17_predictor.mlpackage'),help='Matching source package supplying the input contract')
    args = parser.parse_args()
    def load(path):
        constructor = ct.models.MLModel if path.suffix == '.mlpackage' else ct.models.CompiledMLModel
        return constructor(str(path),compute_units=ct.ComputeUnit.CPU_AND_NE)
    candidate = load(args.candidate)
    baseline = load(args.baseline)
    spec = ct.utils.load_spec(str(args.baseline_package))
    shapes = {i.name:tuple(i.type.multiArrayType.shape) for i in spec.description.input}
    maxima,elapsed,reference_elapsed = [],[],[]
    # Different seeds and a repeated seed test reset reproducibility.
    first_outputs = []
    for seed in [42,7,42]:
        state = candidate.make_state()
        rng = np.random.default_rng(seed)
        keys,values = np.zeros(shapes['keys'],np.float32),np.zeros(shapes['values'],np.float32)
        for step in range(16):
            inputs = {n:np.zeros(s,np.float32) for n,s in shapes.items() if n not in ['keys','values']}
            inputs['embeddings'][:] = rng.normal(0,0.5,size=shapes['embeddings'])
            inputs['write_mask'][:,:,step,:] = 1
            inputs['attention_mask'][...,step+1:] = -np.inf
            # Same valid rotary transform in both executions; not a speech benchmark.
            inputs['cosine'][:] = np.cos(step*0.1)
            inputs['sine'][:] = np.sin(step*0.1)
            start = time.perf_counter_ns()
            expected = baseline.predict(dict(inputs,keys=keys,values=values))
            reference_elapsed.append((time.perf_counter_ns()-start)/1e6)
            keys,values = expected['next_keys'],expected['next_values']
            start = time.perf_counter_ns()
            actual = candidate.predict(inputs,state=state)
            elapsed.append((time.perf_counter_ns()-start)/1e6)
            for name in ['logits','hidden']:
                if not np.isfinite(actual[name]).all():
                    raise AssertionError(f'Non-finite {name} at seed {seed}, step {step}')
                np.testing.assert_allclose(actual[name],expected[name],atol=0.05,rtol=0.03)
                maxima.append(float(np.max(np.abs(actual[name]-expected[name]))))
            if step==0:
                first_outputs.append(actual['logits'].copy())
    np.testing.assert_array_equal(first_outputs[0],first_outputs[2])
    report = {'status':'PASS','scope':'48 synthetic recurrent steps; not speech or end-to-end latency',
        'fresh_session_reproducible':True,'max_abs_error':max(maxima),
        'candidate_median_ms':float(np.median(elapsed)),
        'explicit_cache_median_ms':float(np.median(reference_elapsed))}
    args.report.parent.mkdir(parents=True,exist_ok=True)
    args.report.write_text(json.dumps(report,indent=2)+'\n')
    print(json.dumps(report,indent=2))


if __name__=='__main__':
    main()
