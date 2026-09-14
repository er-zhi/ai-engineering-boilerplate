"""Recurrent Core ML KV feedback parity and warm single-step timing."""
import argparse
import json
import time
from pathlib import Path
import coremltools as ct
import numpy as np
import torch
from qwen_tts import Qwen3TTSModel
from export_qwen_cached_step import controls


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('source',type=Path)
    parser.add_argument('package',type=Path)
    parser.add_argument('--report',type=Path,required=True)
    args = parser.parse_args()
    torch.set_num_threads(4)
    source = Qwen3TTSModel.from_pretrained(str(args.source),device_map='cpu',
        dtype=torch.float32,local_files_only=True,attn_implementation='eager').model.talker.eval()
    model = ct.models.MLModel(str(args.package),compute_units=ct.ComputeUnit.CPU_AND_NE)
    spec = model.get_spec()
    key_shape = list(next(i for i in spec.description.input if i.name=='keys').type.multiArrayType.shape)
    capacity = key_shape[2]
    keys,values = np.zeros(key_shape,np.float32),np.zeros(key_shape,np.float32)
    torch.manual_seed(123)
    sequence = torch.randn(1,capacity,source.config.hidden_size)
    errors,cosines,matches,times = [],[],[],[]
    for position in range(capacity):
        with torch.inference_mode():
            control = controls(source.model,position,capacity)
        inputs = dict(zip(['write_mask','attention_mask','cosine','sine'],[x.numpy() for x in control]))
        inputs.update(embeddings=sequence[:,position:position+1].numpy(),keys=keys,values=values)
        start = time.perf_counter_ns()
        result = model.predict(inputs)
        elapsed = (time.perf_counter_ns()-start)/1e6
        if position >= 5:
            times.append(elapsed)
        keys,values = result['next_keys'],result['next_values']
        if not np.isfinite(keys).all() or not np.isfinite(values).all():
            raise RuntimeError('Non-finite recurrent cache')
        if position in [0,1,7,15,31,capacity-1]:
            with torch.inference_mode():
                expected = source.codec_head(source.model(inputs_embeds=sequence[:,:position+1],use_cache=False).last_hidden_state[:,-1:]).numpy().ravel()
            actual = result['logits'].ravel()
            errors.append(float(np.max(np.abs(actual-expected))))
            cosines.append(float(np.dot(actual,expected)/(np.linalg.norm(actual)*np.linalg.norm(expected))))
            matches.append(bool(actual.argmax()==expected.argmax()))
    report = {'scope':'synthetic recurrent talker decode, not end-to-end TTS',
        'capacity':capacity,'steps':capacity,'max_abs_logit_error':max(errors),
        'minimum_cosine_similarity':min(cosines),'top1_matches':sum(matches),'checked_positions':len(matches),
        'warm_samples':len(times),'warm_step_ms':{'p50':float(np.percentile(times,50)),'p95':float(np.percentile(times,95))},
        'parity_status':'PASS' if min(cosines)>0.999 and all(matches) else 'FAIL'}
    args.report.parent.mkdir(parents=True,exist_ok=True)
    args.report.write_text(json.dumps(report,indent=2)+'\n')
    print(json.dumps(report,indent=2))
    if report['parity_status']!='PASS':
        raise SystemExit(1)


if __name__=='__main__':
    main()
