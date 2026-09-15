"""Hardware integration matrix; no assertion of perceptual quality or 30 ms."""
import argparse
import hashlib
import json
import time
import wave
from pathlib import Path

from voice_stream import VoiceStream, prepare

CASES = [
    ('short', "I'm sorry about the charge. I'll fix it for you."),
    ('technical', 'PostgreSQL, Kubernetes, OAuth, and idempotency require careful configuration.'),
    ('numbers', 'Your refund is $127.45, reference AB-2048, dated September 14, 2026.'),
    ('pauses', "Oh... I understand. Let me check that. Yes — the duplicate charge has been reversed."),
    ('long', 'Before we proceed with the refund, I need to verify which transaction was duplicated, '
        'confirm that your subscription remains active, and explain when the corrected balance will '
        'appear on your statement, so that you do not have to contact us again about the same issue.'),
]


def collect(voice, frames):
    chunks = list(voice.chunks(frames))
    for index, (pcm, meta) in enumerate(chunks):
        if len(pcm) != 3840 or meta['frame'] != index or len(meta['codes']) != 16:
            raise AssertionError('Malformed PCM chunk or code sequence')
    return chunks, dict(voice.last_status)


def identical(left, right):
    return len(left) == len(right) and all(
        a[0] == b[0] and a[1]['codes'] == b[1]['codes']
        for a,b in zip(left,right, strict=True))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('reference', type=Path, help='Runtime JSON identifying explicit model packages')
    parser.add_argument('--gate', type=Path, required=True)
    parser.add_argument('--compiled-dir',type=Path,default=Path('models/compiled-cache'))
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--frontend-assets',type=Path,help='Override source frontend with an exported runtime bundle')
    parser.add_argument('--case',action='append',choices=[name for name,_ in CASES],help='Run selected cases; default all')
    args = parser.parse_args()
    if args.output.exists():
        parser.error('Preserve existing evidence')
    config = json.loads(args.reference.read_text())
    def path(name):
        return Path(config[name]) if config.get(name) else None
    frontend_assets=args.frontend_assets or path('frontend_assets')
    voice = VoiceStream(Path(config['source']),path('packages'),args.gate,CASES[0][1],'',
        compiled_dir=args.compiled_dir,predictor_package=path('predictor_package'),
        prefill_packages=path('prefill_packages'),speaker=config['speaker'],model_prefix='qwen06',
        block_count=1,decoder_package=path('decoder_package'),experimental_history_decoder=True,
        text_projection_package=path('text_projection_package'),long_talker_package=path('long_talker_package'),
        frontend_assets=frontend_assets,
        startup_prefill_packages=path('startup_prefill_packages'))
    args.output.mkdir(parents=True)
    report = {'scope':'Real ANE-admitted runtime stability, not quality acceptance or client latency',
        'reference':str(args.reference),'cross_request_prefix_reuse':False,'audio_cache_used':False,
        'perceptual_quality_validated':False,'cases':[]}
    first_prepared = voice.prepared
    first_result = None
    for name,text in CASES:
        if args.case and name not in args.case:
            continue
        print('CASE '+name,flush=True)
        started = time.perf_counter()
        voice.prepared = (first_prepared if name == 'short' else
            prepare(Path(config['source']),text,'',config['speaker'],True,voice.text_projection,voice.capacity,
                frontend_assets=frontend_assets))
        preparation_ms = (time.perf_counter()-started)*1000
        # Truncation at the selected model capacity is reported, not hidden.
        frames = min(voice.capacity-10,voice.capacity-voice.prepared[0]['inputs_embeds'].shape[1])
        expected,status = collect(voice,frames)
        exact_prefill_reference = None
        if voice.startup_prefill_blocks is not None:
            startup_prefill = voice.startup_prefill_blocks
            try:
                voice.startup_prefill_blocks = None
                reference, reference_status = collect(voice, frames)
            finally:
                voice.startup_prefill_blocks = startup_prefill
            exact_prefill_reference = identical(expected, reference) and (
                status['ended_by_eos'] == reference_status['ended_by_eos']
            )
            if not exact_prefill_reference:
                raise AssertionError('Dual prefill differs from exact full prefill: '+name)
        repeated,repeated_status = collect(voice,frames)
        if not expected or not identical(expected,repeated) or status['ended_by_eos'] != repeated_status['ended_by_eos']:
            raise AssertionError('Non-repeatable complete generation: '+name)
        cancellations = []
        for stop in ([1,16,73,118,120] if voice.capacity>128 else [1,16,73]):
            if stop >= len(expected):
                continue
            stream = voice.chunks(frames)
            try:
                partial = [next(stream) for _ in range(stop)]
            finally:
                stream.close()
            restarted = voice.chunks(frames)
            try:
                new_first = [next(restarted)]
            finally:
                restarted.close()
            if not identical(partial,expected[:stop]) or not identical(new_first,expected[:1]):
                raise AssertionError('Cancellation leaked generation state: '+name)
            cancellations.append(stop)
        filename = args.output/(name+'.wav')
        payload = b''.join(pcm for pcm,_ in expected)
        with wave.open(str(filename),'wb') as wav:
            wav.setparams((1,2,24000,0,'NONE','not compressed'))
            wav.writeframes(payload)
        row = {'name':name,'text':text,'frames':len(expected),'status':status,
            'preparation_including_model_load_ms':preparation_ms,'repeated_pcm_exact':True,
            'dual_vs_full_prefill_pcm_exact':exact_prefill_reference,
            'cancel_after_frames_verified':cancellations,'sha256_pcm':hashlib.sha256(payload).hexdigest(),
            'chunks':[meta for _,meta in expected],'wav':str(filename)}
        report['cases'].append(row)
        (args.output/'report.json').write_text(json.dumps(report,indent=2)+'\n')
        print(json.dumps({k:v for k,v in row.items() if k!='chunks'}),flush=True)
        if name == 'short':
            first_result = expected
    report['cross_text_restore_exact'] = None
    if first_result is not None:
        voice.prepared = first_prepared
        restored,_ = collect(voice,voice.capacity-10)
        if not identical(restored,first_result):
            raise AssertionError('Different intervening texts polluted original session')
        report['cross_text_restore_exact'] = True
    report['status'] = 'PASS_EXECUTED_CHECKS'
    report['limitations'] = ['Not an HTTP cancellation test','No native emotion control',
        'Long speech may truncate at fixed KV capacity','No perceptual quality or pronunciation validation']
    (args.output/'report.json').write_text(json.dumps(report,indent=2)+'\n')


if __name__ == '__main__':
    main()
