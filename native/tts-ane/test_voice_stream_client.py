"""Receive real HTTP PCM chunks and save an independently received WAV."""
import argparse
import json
import time
import urllib.request
import wave
from pathlib import Path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output',type=Path,required=True)
    parser.add_argument('--port',type=int,default=8765)
    args = parser.parse_args()
    if args.output.exists() or args.output.with_suffix('.json').exists():
        parser.error('Use new output paths; preserve earlier recordings')
    args.output.parent.mkdir(parents=True,exist_ok=True)
    timings = []
    started = time.perf_counter()
    with urllib.request.urlopen(f'http://127.0.0.1:{args.port}/stream',timeout=180) as response:
        if response.headers.get('Transfer-Encoding')!='chunked':
            raise RuntimeError('Expected chunked HTTP response')
        if not response.headers.get('Content-Type','').startswith('audio/pcm'):
            raise RuntimeError('Expected raw PCM, not a WAV header or error body')
        headers_ms = (time.perf_counter()-started)*1000
        first_byte = response.read(1)
        first_pcm_byte_ms = (time.perf_counter()-started)*1000
        if not first_byte:
            raise RuntimeError('No PCM body received')
        with wave.open(str(args.output),'wb') as wav:
            wav.setnchannels(1)
            wav.setsampwidth(2)
            wav.setframerate(24000)
            while True:
                payload = (first_byte + response.read(3839)) if first_byte else response.read(3840)
                first_byte = b''
                if not payload:
                    break
                if len(payload)%2:
                    raise RuntimeError('Truncated PCM sample')
                timings.append({'received_ms':(time.perf_counter()-started)*1000,'bytes':len(payload)})
                wav.writeframesraw(payload)
    if not timings:
        raise RuntimeError('No audio received')
    report = {'request_count':1,'first_audio_ms':timings[0]['received_ms'],
        'headers_ms':headers_ms,'first_pcm_byte_ms':first_pcm_byte_ms,
        'scope':'Loopback GET to first raw PCM body byte; fixed preconfigured text, not fresh-text processing or audible speech onset',
        'chunks':timings,'elapsed_ms':(time.perf_counter()-started)*1000,
        'p95_established':False,'audio_seconds':sum(x['bytes'] for x in timings)/48000}
    args.output.with_suffix('.json').write_text(json.dumps(report,indent=2)+'\n')
    print(json.dumps({k:v for k,v in report.items() if k!='chunks'},indent=2))


if __name__=='__main__':
    main()
