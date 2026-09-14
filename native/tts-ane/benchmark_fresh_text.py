"""Client latency for new-text POST requests, including server preparation."""
import argparse
import hashlib
import http.client
import json
from pathlib import Path
import time
import wave
import numpy as np


TEXTS = ["I'm sorry about the charge. I'll fix it for you.",
    'Please check the PostgreSQL replication configuration.',
    'Your refund is $127.45, reference AB-2048, dated September 14, 2026.',
    'Before we proceed with the refund, I need to verify which transaction was duplicated, '
    'confirm that your subscription remains active, and explain when the corrected balance '
    'will appear on your statement, so that you do not have to contact us again about the same issue.']


def request(port,text,frames,abort=False):
    connection = http.client.HTTPConnection('127.0.0.1',port,timeout=60)
    body = json.dumps({'text':text,'max_frames':frames})
    started = time.perf_counter()
    try:
        connection.request('POST','/stream',body,{'Content-Type':'application/json'})
        response = connection.getresponse()
        headers_ms = (time.perf_counter()-started)*1000
        if response.status != 200:
            raise RuntimeError(f'HTTP {response.status}: {response.read(2048)!r}')
        first = response.read(1)
        first_ms = (time.perf_counter()-started)*1000
        if not first:
            raise ValueError('Empty PCM stream')
        payload = first if abort else first+response.read()
        return payload,{'client_first_pcm_ms':first_ms,'headers_ms':headers_ms,
            'server_first_pcm_ms':float(response.getheader('X-TTS-First-PCM-Ms')),
            'preparation_ms':float(response.getheader('X-TTS-Preparation-Ms'))}
    finally:
        connection.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--port',type=int,default=8765)
    parser.add_argument('--trials',type=int,default=100)
    parser.add_argument('--reference-wav',type=Path,required=True)
    parser.add_argument('--output',type=Path,required=True)
    args = parser.parse_args()
    if args.output.exists() or not 1 <= args.trials <= 1000:
        parser.error('Use new output and 1..1000 trials')
    with wave.open(str(args.reference_wav),'rb') as wav:
        reference = wav.readframes(wav.getnframes())
    full,_ = request(args.port,TEXTS[0],502)
    if full != reference:
        raise AssertionError('Fresh text output differs from configured-text WAV')
    rows = []
    first_hashes = {}
    for trial in range(args.trials+5):
        index = trial%len(TEXTS)
        pcm,timing = request(args.port,TEXTS[index],1)
        if len(pcm) != 3840:
            raise AssertionError('One-frame request returned incorrect PCM size')
        digest = hashlib.sha256(pcm).hexdigest()
        if index in first_hashes and first_hashes[index] != digest:
            raise AssertionError('First PCM changes after other text requests')
        first_hashes[index] = digest
        if trial >= 5:
            rows.append(dict(timing,text_index=index,text_chars=len(TEXTS[index])))
    request(args.port,TEXTS[2],502,abort=True)
    recovered,_ = request(args.port,TEXTS[0],502)
    if recovered != reference:
        raise AssertionError('HTTP disconnect polluted subsequent complete generation')
    report = {'scope':'Warm loaded server, new full text each POST; TCP, HTTP, text preparation and first PCM included',
        'text_input_streaming':False,'audio_streaming':True,'model_loading_excluded':True,
        'audio_cache_used':False,'cross_request_prefix_reuse':False,'trials':args.trials,
        'texts':TEXTS,'configured_vs_fresh_full_pcm_exact':True,'http_disconnect_then_full_pcm_exact':True,
        'timings_ms':{name:{'p50':float(np.percentile([row[name] for row in rows],50)),
            'p95':float(np.percentile([row[name] for row in rows],95))}
            for name in ['client_first_pcm_ms','server_first_pcm_ms','preparation_ms']},'raw':rows,
        'limitations':['First PCM is not necessarily audible speech onset',
            'One-frame requests isolate startup; not sustained long-stream performance',
            'No quality approval or 30 ms guarantee']}
    args.output.write_text(json.dumps(report,indent=2)+'\n')
    print(json.dumps({k:v for k,v in report.items() if k!='raw'},indent=2),flush=True)


if __name__ == '__main__':
    main()
