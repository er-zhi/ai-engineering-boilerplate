"""Validate fixed-frame decoder parity and warm component latency (not TTS TTFA)."""
import argparse
import json
import platform
import time
from pathlib import Path

import coremltools as ct
import numpy as np
import torch
from export_qwen12hz_decoder import OneFrameDecoder, replace_nonoverlap_transconvs
from qwen_tts.core.tokenizer_12hz.modeling_qwen3_tts_tokenizer_v2 import Qwen3TTSTokenizerV2Model


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('source', type=Path)
    parser.add_argument('package', type=Path)
    parser.add_argument('--report', type=Path, required=True)
    parser.add_argument('--frames', type=int, default=1)
    parser.add_argument('--tail',action='store_true',help='Verify the last frame of full history windows, not an independent first frame')
    parser.add_argument('--codes-report', type=Path, help='Use measured first-frame codes or all chunk codes from a speech report')
    args = parser.parse_args()
    if not 1 <= args.frames <= 64:
        parser.error('frames must be between 1 and 64')
    torch.set_num_threads(4)
    torch.manual_seed(42)
    tokenizer = Qwen3TTSTokenizerV2Model.from_pretrained(
        args.source / 'speech_tokenizer', torch_dtype=torch.float32,
        local_files_only=True).eval()
    wrapper = OneFrameDecoder(tokenizer, latent_input=True).eval()
    with torch.inference_mode():
        # Use actual codebook outputs, rather than arbitrary latent magnitudes.
        inputs = [tokenizer.decoder.quantizer.decode(torch.randint(0, 1024, (1, 16, args.frames if args.tail else 1))) for _ in range(8)]
        if args.codes_report:
            recorded = json.loads(args.codes_report.read_text())
            cases = ([recorded['first_frame_codes']] if 'first_frame_codes' in recorded
                     else recorded['codes'] if 'codes' in recorded
                     else [chunk['codes'] for chunk in recorded['chunks']])
            if not 1 <= len(cases) <= 1024:
                raise ValueError('Expected 1..1024 recorded frames')
            for codes in cases:
                if len(codes) != 16 or any(type(x) is not int or not 0 <= x < 2048 for x in codes):
                    raise ValueError('Expected 16 integer audio codes in [0, 2048)')
            if args.tail:
                if len(cases) < args.frames:
                    raise ValueError('Insufficient recorded frames for a full tail window')
                inputs = [tokenizer.decoder.quantizer.decode(torch.tensor(cases[end-args.frames:end]).T.unsqueeze(0))
                          for end in range(args.frames,len(cases)+1)]
            else:
                inputs = [tokenizer.decoder.quantizer.decode(torch.tensor(codes).reshape(1,16,1)) for codes in cases]
        references = [wrapper(x).numpy()[..., -1920:] if args.tail else wrapper(x).numpy() for x in inputs]
        replace_nonoverlap_transconvs(tokenizer.decoder)
        rewritten = [wrapper(x).numpy()[..., -1920:] if args.tail else wrapper(x).numpy() for x in inputs]
    rewrite_error = max(float(np.max(np.abs(a-b))) for a,b in zip(references, rewritten))
    if rewrite_error > 1e-4:
        raise RuntimeError(f'Rewrite parity failed: {rewrite_error}')
    model = ct.models.MLModel(str(args.package), compute_units=ct.ComputeUnit.CPU_AND_NE)
    def predict_input(x):
        return {'latent': x.numpy() if args.tail else np.pad(x.numpy(), ((0,0),(0,0),(0,args.frames-1)))}
    errors = []
    for x, expected in zip(inputs, references):
        actual = model.predict(predict_input(x))['pcm'][..., -1920:] if args.tail else model.predict(predict_input(x))['pcm'][..., :1920]
        if actual.shape != expected.shape or not np.isfinite(actual).all():
            raise RuntimeError('Invalid output')
        errors.append(float(np.max(np.abs(actual-expected))))
    cpu_error = None
    if max(errors) > 0.02:
        cpu = ct.models.MLModel(str(args.package), compute_units=ct.ComputeUnit.CPU_ONLY)
        cpu_error = max(float(np.max(np.abs(cpu.predict(predict_input(x))['pcm'][..., :1920]-expected))) for x,expected in zip(inputs,references))
        print(json.dumps({'cpu_only_max_abs_error': cpu_error, 'cpu_and_ne_max_abs_error': max(errors), 'rewrite_max_abs_error': rewrite_error}), flush=True)
    durations = []
    for i in range(110):
        x = predict_input(inputs[i % len(inputs)])
        start = time.perf_counter_ns()
        model.predict(x)
        elapsed = (time.perf_counter_ns() - start) / 1e6
        if i >= 10:
            durations.append(elapsed)
    report = {'scope': 'last frame of full history windows; NOT end-to-end streaming' if args.tail else 'independent single-frame latent decoder; NOT end-to-end streaming',
        'parity_status': 'PASS' if max(errors) <= 0.02 else 'FAIL',
        'max_abs_error_threshold': 0.02, 'cpu_only_max_abs_error': cpu_error,
        'platform': platform.platform(), 'machine': platform.machine(),
        'compute_units': 'CPU_AND_NE', 'samples': len(durations),
        'decoder_input_frames': args.frames,
        'tail_window_test':args.tail,
        'input_cases': len(inputs), 'codes_report': str(args.codes_report) if args.codes_report else None,
        'rewrite_max_abs_error': rewrite_error, 'coreml_max_abs_error': max(errors),
        'warm_prediction_ms': {q: float(np.percentile(durations, p)) for q,p in [('p50',50),('p95',95),('p99',99)]},
        'pcm_shape': list(references[0].shape), 'sample_rate': 24000}
    args.report.parent.mkdir(parents=True, exist_ok=True)
    args.report.write_text(json.dumps(report, indent=2) + '\n')
    print(json.dumps(report, indent=2))
    if report['parity_status'] != 'PASS':
        raise SystemExit(1)


if __name__ == '__main__':
    main()
