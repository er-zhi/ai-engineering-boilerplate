"""Validate speaker-prefix KV reuse across texts; never an audio/TTFA benchmark."""
import copy
import argparse
import hashlib
import json
from pathlib import Path
import time

import numpy as np
import torch
from qwen_tts import Qwen3TTSModel
from benchmark_fused_predictor import load
from export_qwen_cached_step import controls


def main():
    root = Path(__file__).resolve().parent
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('source',type=Path,help='Official Qwen3-TTS 0.6B CustomVoice checkpoint')
    parser.add_argument('--output',type=Path,default=root/'reports/invariant-prefix-probe.json')
    parser.add_argument('--package',type=Path,default=root/'models/qwen06-stateful-fp16/qwen06_cached_block0.mlpackage')
    parser.add_argument('--capacity',type=int,choices=[16,32,64,128],default=128)
    parser.add_argument('--migration-reference',type=Path,help='Diagnostic: compare short prefix and state migration against a 128-position model')
    args = parser.parse_args()
    output = args.output
    if output.exists():
        raise ValueError('Preserve previous probe')
    torch.set_num_threads(4)
    tts = Qwen3TTSModel.from_pretrained(str(args.source),
        dtype=torch.float32,device_map='cpu',local_files_only=True,attn_implementation='eager')
    talker = tts.model.talker.eval()
    phrases = ["I'm sorry about the charge. I'll fix it for you.",
        "Thank you for waiting. Your refund is on its way.",
        "Could you confirm the last four digits, please?"]
    captured = []
    class Captured(Exception):
        pass
    original_generate = talker.generate
    def capture(**kwargs):
        captured.append(kwargs['inputs_embeds'].detach().clone())
        raise Captured()
    talker.generate = capture
    try:
        for text in phrases:
            try:
                tts.generate_custom_voice(text=text,language='English',speaker='Serena',non_streaming_mode=False)
            except Captured:
                pass
    finally:
        talker.generate = original_generate
    if len(captured) != len(phrases):
        raise ValueError('Every phrase must be captured')
    prefix = captured[0][:,:-1]
    if not all(torch.equal(prefix, value[:,:-1]) for value in captured):
        raise ValueError('Prefix is text-dependent; do not reuse it')
    if len({value[:,-1:].numpy().tobytes() for value in captured}) != len(phrases):
        raise ValueError('Probe needs different text-dependent first embeddings')
    source_results = []
    with torch.inference_mode():
        prefix_result = talker.model(inputs_embeds=prefix,use_cache=True)
        for value in captured:
            fresh = talker.codec_head(talker.model(inputs_embeds=value,use_cache=False).last_hidden_state[:,-1:])
            cached = talker.codec_head(talker.model(inputs_embeds=value[:,-1:],
                past_key_values=copy.deepcopy(prefix_result.past_key_values),use_cache=True).last_hidden_state)
            torch.testing.assert_close(cached,fresh,atol=1e-3,rtol=1e-3)
            source_results.append({'peak_logit_error':float((cached-fresh).abs().max()),
                'fresh_token':int(fresh.argmax(-1)), 'cached_token':int(cached.argmax(-1))})
    model,admission = load(args.package,
        root/'models/compiled-cache',Path('/tmp/ane-gate'))
    with torch.inference_mode():
        control = [{name:value.numpy().copy() for name,value in zip(
            ['write_mask','attention_mask','cosine','sine'],controls(talker.model,i,args.capacity))}
            for i in range(captured[0].shape[1])]
    values = [value.numpy().copy() for value in captured]
    shared = model.make_state()
    for i in range(prefix.shape[1]):
        model.predict(dict(control[i],embeddings=values[0][:,i:i+1]),state=shared)
    position = prefix.shape[1]
    cases = []
    for text,value in zip(phrases,values):
        fresh_state = model.make_state()
        for i in range(value.shape[1]):
            fresh = model.predict(dict(control[i],embeddings=value[:,i:i+1]),state=fresh_state)
        cached = model.predict(dict(control[position],embeddings=value[:,-1:]),state=shared)
        np.testing.assert_array_equal(cached['logits'],fresh['logits'])
        np.testing.assert_array_equal(cached['hidden'],fresh['hidden'])
        cases.append({'text':text,'exact_coreml_match':True,'first_token':int(cached['logits'].argmax(-1).item())})
    # Prove that logical reset does not expose prior request tokens in later slots.
    for name in ['key_cache','value_cache']:
        state_values = shared.read_state(name).astype(np.float32)
        state_values[:,:,position+1:,:] = 1.0
        shared.write_state(name,state_values)
    value = values[-1]
    poisoned = model.predict(dict(control[position],embeddings=value[:,-1:]),state=shared)
    np.testing.assert_array_equal(poisoned['logits'],fresh['logits'])
    np.testing.assert_array_equal(poisoned['hidden'],fresh['hidden'])
    timings = []
    for i in range(105):
        value = values[i%len(values)]
        started = time.perf_counter()
        result = model.predict(dict(control[position],embeddings=value[:,-1:]),state=shared)
        elapsed = (time.perf_counter()-started)*1000
        if not np.isfinite(result['logits']).all():
            raise ValueError('Non-finite logits')
        if i>=5:
            timings.append(elapsed)
    record = {'scope':'one talker step with invariant prefix; excludes text frontend, predictor, decoder and transport; NOT TTFA',
        'package':str(args.package.resolve()),
        'cache_capacity':args.capacity,
        'invariant_tokens':int(position),'prefix_sha256':hashlib.sha256(prefix.numpy().tobytes()).hexdigest(),
        'source_fp32':source_results,'coreml_cases':cases,'admission':admission,
        'warm_trials':len(timings),'step_p50_ms':float(np.percentile(timings,50)),
        'step_p95_ms':float(np.percentile(timings,95)),'raw_step_ms':timings,
        'audio_cached':False,'runtime_integrated':False}
    record['masked_future_state_independence'] = True
    if args.migration_reference:
        reference,reference_admission = load(args.migration_reference,root/'models/compiled-cache',Path('/tmp/ane-gate'))
        with torch.inference_mode():
            long_controls = [{name:value.numpy().copy() for name,value in zip(
                ['write_mask','attention_mask','cosine','sine'],controls(talker.model,i,128))}
                for i in range(20)]
        migration_cases = []
        for value in values:
            short_state,long_state = model.make_state(),reference.make_state()
            prefix_errors = []
            for i in range(value.shape[1]):
                embedding = value[:,i:i+1]
                short_result = model.predict(dict(control[i],embeddings=embedding),state=short_state)
                long_result = reference.predict(dict(long_controls[i],embeddings=embedding),state=long_state)
                prefix_errors.append({name:float(np.abs(short_result[name]-long_result[name]).max()) for name in ['logits','hidden']})
            migrated = reference.make_state()
            for name in ['key_cache','value_cache']:
                source = short_state.read_state(name)
                target = np.zeros_like(long_state.read_state(name),dtype=np.float32)
                target[:,:,:value.shape[1],:] = source[:,:,:value.shape[1],:]
                migrated.write_state(name,target)
            continuation_errors = []
            for i in range(value.shape[1],20):
                # Fixed identical diagnostic inputs, not free-running speech acceptance.
                inputs = dict(long_controls[i],embeddings=value[:,-1:])
                actual = reference.predict(inputs,state=migrated)
                expected = reference.predict(inputs,state=long_state)
                continuation_errors.append({name:float(np.abs(actual[name]-expected[name]).max()) for name in ['logits','hidden']})
            migration_cases.append({'prefix_errors':prefix_errors,'continuation_errors':continuation_errors,
                'exact':all(error==0 for row in prefix_errors+continuation_errors for error in row.values())})
        record['migration'] = {'scope':'Three captured prefixes and ten fixed-input continuation steps each; not full speech quality',
            'reference':str(args.migration_reference),'admission':reference_admission,'cases':migration_cases}
        # Identical text-dependent steps; alternate order to reduce thermal/order bias.
        paired = {'short':[], 'full':[]}
        pair_states = {'short':model.make_state(), 'full':reference.make_state()}
        pair_models = {'short':model, 'full':reference}
        pair_controls = {'short':control, 'full':long_controls}
        for name in pair_models:
            for i in range(position):
                pair_models[name].predict(dict(pair_controls[name][i],embeddings=values[0][:,i:i+1]),state=pair_states[name])
        for trial in range(110):
            results = {}
            for name in (['short','full'] if trial % 2 else ['full','short']):
                inputs = dict(pair_controls[name][position],embeddings=values[trial % len(values)][:,-1:])
                started = time.perf_counter()
                results[name] = pair_models[name].predict(inputs,state=pair_states[name])
                duration = (time.perf_counter()-started)*1000
                if trial >= 10:
                    paired[name].append(duration)
            for name in ['logits','hidden']:
                np.testing.assert_array_equal(results['short'][name],results['full'][name])
        record['paired_cache_timing'] = {
            'scope':'Alternating one-step short/full cache; three prepared texts, excludes predictor/PCM and migration',
            'trials_per_model':100,'warmups_per_model':10,'exact_outputs':True,
            'timings':{name:{'p50_ms':float(np.percentile(rows,50)),
                'p95_ms':float(np.percentile(rows,95)),'raw_ms':rows} for name,rows in paired.items()}}
    output.parent.mkdir(parents=True,exist_ok=True)
    output.write_text(json.dumps(record,indent=2)+'\n')
    print(json.dumps({k:v for k,v in record.items() if k not in ['raw_step_ms','admission']}),flush=True)


if __name__ == '__main__':
    main()
