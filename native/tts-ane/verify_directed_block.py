"""Fail closed on CPU fallback and measure a VoiceDesign block against FP32."""
import argparse
import json
import subprocess
import time
from pathlib import Path
import coremltools as ct
import numpy as np


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('package',type=Path)
    parser.add_argument('--gate',type=Path,required=True)
    parser.add_argument('--report',type=Path,required=True)
    args = parser.parse_args()
    process = subprocess.run([str(args.gate),'--min-ane-operation-ratio','0',str(args.package)],
        capture_output=True,text=True,check=True)
    gate = json.loads(process.stdout)
    report = {'compute_plan':gate,'status':'FAIL'}
    args.report.parent.mkdir(parents=True,exist_ok=True)
    if gate['ane_operation_ratio'] != 1:
        args.report.write_text(json.dumps(report,indent=2)+'\n')
        raise SystemExit('CPU fallback; refusing timing run')
    fixture = np.load(args.package.with_suffix('.inputs.npz'))
    model = ct.models.MLModel(str(args.package),compute_units=ct.ComputeUnit.CPU_AND_NE)
    times = []
    for i in range(35):
        start = time.perf_counter_ns()
        output = model.predict({'embeddings':fixture['embeddings']})
        elapsed = (time.perf_counter_ns()-start)/1e6
        if i>=5:
            times.append(elapsed)
    actual = output.get('logits',output['hidden']).ravel()
    expected = fixture['expected'].ravel()
    finite = bool(np.isfinite(actual).all())
    cosine = float(np.dot(actual,expected)/(np.linalg.norm(actual)*np.linalg.norm(expected))) if finite else None
    report.update(status='PASS' if cosine is not None and cosine>0.999 else 'FAIL',cosine=cosine,
        output_finite=finite,max_abs_error=float(np.max(np.abs(actual-expected))) if finite else None,
        warm_ms={'p50':float(np.percentile(times,50)),'p95':float(np.percentile(times,95))},samples=len(times))
    args.report.write_text(json.dumps(report,indent=2)+'\n')
    print(json.dumps(report,indent=2))
    if report['status']!='PASS':
        raise SystemExit(1)


if __name__=='__main__':
    main()
