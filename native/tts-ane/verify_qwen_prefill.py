"""Check Core ML prefill logits against the unmodified upstream forward path."""
import argparse
import json
import time
from pathlib import Path
import coremltools as ct
import numpy as np
import torch
from qwen_tts import Qwen3TTSModel


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('source', type=Path)
    parser.add_argument('package', type=Path)
    parser.add_argument('--report', type=Path, required=True)
    args = parser.parse_args()
    torch.set_num_threads(4)
    model = ct.models.MLModel(str(args.package), compute_units=ct.ComputeUnit.CPU_AND_NE)
    source = Qwen3TTSModel.from_pretrained(str(args.source), device_map='cpu',
        dtype=torch.float32, local_files_only=True, attn_implementation='eager').model.talker.eval()
    length = model.get_spec().description.input[0].type.multiArrayType.shape[1]
    torch.manual_seed(42)
    errors, similarities, top1 = [], [], []
    for _ in range(4):
        x = torch.randn(1, length, source.config.hidden_size)
        with torch.inference_mode():
            expected = source.codec_head(source.model(inputs_embeds=x, use_cache=False).last_hidden_state[:, -1:, :]).numpy().flatten()
        actual = model.predict({'embeddings': x.numpy()})['logits'].flatten()
        if not np.isfinite(actual).all():
            raise RuntimeError('Non-finite logits')
        errors.append(float(np.max(np.abs(expected-actual))))
        similarities.append(float(np.dot(expected,actual)/(np.linalg.norm(expected)*np.linalg.norm(actual))))
        top1.append(bool(expected.argmax() == actual.argmax()))
    times = []
    for i in range(30):
        start = time.perf_counter_ns()
        model.predict({'embeddings': x.numpy()})
        if i >= 5:
            times.append((time.perf_counter_ns()-start)/1e6)
    report = {'scope': 'prefill on seeded synthetic embeddings, not generation or TTFA',
        'max_abs_logit_error': max(errors), 'minimum_cosine_similarity': min(similarities),
        'top1_matches': sum(top1), 'cases': len(top1), 'samples': len(times),
        'warm_prediction_ms': {'p50': float(np.percentile(times,50)), 'p95': float(np.percentile(times,95))},
        'parity_status': 'PASS' if min(similarities) > 0.999 and all(top1) else 'FAIL'}
    args.report.parent.mkdir(parents=True, exist_ok=True)
    args.report.write_text(json.dumps(report, indent=2)+'\n')
    print(json.dumps(report, indent=2))
    if report['parity_status'] != 'PASS':
        raise SystemExit(1)


if __name__ == '__main__':
    main()
