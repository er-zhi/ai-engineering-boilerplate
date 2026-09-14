"""Compare recurrent Core ML PCM against full source decoding on identical codes."""
import argparse
import copy
import json
from pathlib import Path
import time

import numpy as np
import soundfile as sf
import torch
from benchmark_fused_predictor import load
from export_streaming_decoder import controls, Qwen3TTSTokenizerV2Model, StreamingDecoder


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('model', type=Path)
    parser.add_argument('codes', type=Path)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--explicit-state', action='store_true')
    parser.add_argument('--teacher-state', action='store_true', help='Diagnostic only: exact FP32 reference histories each frame')
    parser.add_argument('--diagnostic-cpu', action='store_true', help='Numerical isolation only, never ANE admission or production timing')
    args = parser.parse_args()
    if args.output.exists() or args.output.with_suffix('.wav').exists():
        parser.error('Choose new output paths')
    root = Path(__file__).resolve().parent
    torch.set_num_threads(4)
    decoder = Qwen3TTSTokenizerV2Model.from_pretrained(
        root.parent/'tts-bakeoff/qwen06-customvoice/speech_tokenizer',
        dtype=torch.float32, local_files_only=True).eval().decoder
    record = json.loads(args.codes.read_text())
    codes = torch.tensor([chunk['codes'] for chunk in record['chunks']]).T.unsqueeze(0)
    with torch.inference_mode():
        reference = decoder(codes)[0, 0].numpy()
        latents = decoder.quantizer.decode(codes)
        inputs = []
        for i in range(codes.shape[-1]):
            values = (latents[..., i:i+1], *controls(decoder, i))
            inputs.append({name: value.numpy().astype(np.float32) for name, value in zip(
                ['latent', 'cosine', 'sine', 'attention_mask'], values)})
    if args.diagnostic_cpu:
        import coremltools as ct
        model = ct.models.MLModel(str(args.model), compute_units=ct.ComputeUnit.CPU_ONLY)
        admission = {'status': 'NOT_ANE_DIAGNOSTIC_CPU_ONLY'}
    else:
        model, admission = load(args.model, root/'models/compiled-cache', Path('/tmp/ane-gate'))
    stage_reference = None
    stage_errors = {}
    if args.explicit_state:
        import coremltools as ct
        spec = ct.models.MLModel(str(args.model), skip_model_load=True).get_spec()
        if any(feature.name.startswith('debug_') for feature in spec.description.output):
            stage_reference = StreamingDecoder(copy.deepcopy(decoder)).eval()
            stage_reference.debug = True
            stage_errors = {name: [] for name in stage_reference.debug_names}
        shapes = {feature.name: tuple(feature.type.multiArrayType.shape)
            for feature in spec.description.input if feature.name.startswith('in_')}
        state = {name: np.zeros(shape, np.float32) for name, shape in shapes.items()}
    else:
        state = model.make_state()
    if args.teacher_state and (not args.explicit_state or stage_reference is None):
        parser.error('Teacher states require an explicit-state stage-debug model')
    chunks, timings = [], []
    for sample in inputs:
        if args.teacher_state:
            state = {'in_'+name.replace('.', '_'): value.detach().numpy().copy()
                for name, value in stage_reference.states()}
        start = time.perf_counter()
        result = model.predict({**sample, **state}) if args.explicit_state else model.predict(sample, state=state)
        timings.append((time.perf_counter()-start)*1000)
        chunks.append(result['pcm'].reshape(-1).copy())
        if stage_reference is not None:
            with torch.inference_mode():
                expected_stages = stage_reference(*(torch.from_numpy(sample[name]) for name in
                    ['latent','cosine','sine','attention_mask']))[1:]
            for name, expected in zip(stage_reference.debug_names, expected_stages):
                expected = expected.numpy()
                observed = result['debug_'+name]
                delta = observed-expected
                stage_errors[name].append({'peak_error': float(np.abs(delta).max()),
                    'rms_error': float(np.sqrt(np.mean(delta**2))),
                    'reference_peak': float(np.abs(expected).max()),
                    'reference_rms': float(np.sqrt(np.mean(expected**2)))})
        if args.explicit_state:
            state = {name: result['out_'+name[3:]] for name in shapes}
    actual = np.concatenate(chunks)
    if actual.shape != reference.shape or not np.isfinite(actual).all():
        raise ValueError('Invalid PCM')
    error = actual-reference
    output = {'admission': admission, 'frames': len(inputs),
        'max_abs_error': float(np.max(np.abs(error))),
        'rms_error': float(np.sqrt(np.mean(error**2))),
        'frame_max_errors': [float(np.max(np.abs(error[i:i+1920]))) for i in range(0,len(error),1920)],
        'decode_ms': timings, 'scope': 'decoder only; not full TTS TTFA',
        'stage_errors': stage_errors,
        'teacher_state_diagnostic': args.teacher_state,
        'cpu_only_diagnostic': args.diagnostic_cpu,
        'within_existing_peak_0_02_limit': bool(np.max(np.abs(error)) <= .02)}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(output, indent=2)+'\n')
    sf.write(args.output.with_suffix('.wav'), actual, 24000, subtype='PCM_16')
    print(json.dumps({k:v for k,v in output.items() if k not in ['frame_max_errors','decode_ms','admission','stage_errors']}), flush=True)


if __name__ == '__main__':
    main()
