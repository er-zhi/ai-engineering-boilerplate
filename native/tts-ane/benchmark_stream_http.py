"""Measure real loopback PCM body delivery; never treat headers as audio."""
import argparse
import hashlib
import http.client
import json
from pathlib import Path
import time
import wave

import numpy as np


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--port', type=int, default=8765)
    parser.add_argument('--trials', type=int, default=100)
    parser.add_argument('--reference-wav', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    if args.output.exists() or not 1 <= args.trials <= 1000:
        parser.error('Require a new output path and 1..1000 trials')
    with wave.open(str(args.reference_wav), 'rb') as wav:
        if (wav.getframerate(), wav.getnchannels(), wav.getsampwidth()) != (24000, 1, 2):
            raise ValueError('Expected 24 kHz mono PCM16 reference')
        reference = wav.readframes(wav.getnframes())
    expected_hash = hashlib.sha256(reference).hexdigest()
    rows = []
    for trial in range(args.trials + 5):
        connection = http.client.HTTPConnection('127.0.0.1', args.port, timeout=30)
        try:
            start = time.perf_counter()
            connection.request('GET', '/stream')
            response = connection.getresponse()
            headers_ms = (time.perf_counter() - start) * 1000
            if response.status != 200 or not response.getheader('Content-Type', '').startswith('audio/pcm'):
                raise RuntimeError('Expected a successful raw PCM response')
            first = response.read(1)
            first_ms = (time.perf_counter() - start) * 1000
            if not first:
                raise RuntimeError('Empty PCM response')
            # Bound the read and require exact EOS length, not just a matching prefix.
            pcm = first + response.read(len(reference))
            if len(pcm) != len(reference) or hashlib.sha256(pcm).hexdigest() != expected_hash:
                raise RuntimeError('Received PCM differs from reference')
            row = {'trial': trial, 'warmup': trial < 5,
                   'headers_ms': headers_ms, 'first_pcm_byte_ms': first_ms,
                   'complete_ms': (time.perf_counter() - start) * 1000}
            rows.append(row)
            print(json.dumps(row), flush=True)
        finally:
            connection.close()
    measured = [row['first_pcm_byte_ms'] for row in rows if not row['warmup']]
    report = {'scope': 'Sequential warm loopback requests, fixed prepared text; excludes fresh-text preparation, cold load, speech onset and remote transport',
              'audio_cache_used': False, 'trials': args.trials, 'warmups': 5,
              'new_connection_per_request': True, 'all_pcm_matches_reference': True,
              'reference_pcm_sha256': expected_hash,
              'p50_ms': float(np.percentile(measured, 50)),
              'p95_ms': float(np.percentile(measured, 95)), 'rows': rows}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + '\n')
    print(json.dumps({k: v for k, v in report.items() if k != 'rows'}), flush=True)


if __name__ == '__main__':
    main()
