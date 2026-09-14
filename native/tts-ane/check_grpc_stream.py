"""Receive PCM before sending the rest of the text; compare with reference WAV."""
import argparse
import asyncio
import hashlib
import json
import time
import wave
from pathlib import Path
import grpc
import tts_stream_pb2 as pb
import tts_stream_pb2_grpc as rpc


async def check(args):
    async with grpc.aio.insecure_channel(args.address) as channel:
        await channel.channel_ready()
        call = rpc.SpeechStub(channel).Synthesize(timeout=60)
        started = time.perf_counter()
        await call.write(pb.TextPart(text="I'm "))
        first = await call.read()
        elapsed = (time.perf_counter() - started) * 1000
        if first is grpc.aio.EOF or not first.pcm_s16le:
            raise RuntimeError('No first PCM')
        await call.write(pb.TextPart(text="sorry about the charge. I'll fix it for you."))
        await call.done_writing()
        chunks = [first.pcm_s16le]
        while True:
            chunk = await call.read()
            if chunk is grpc.aio.EOF:
                break
            if chunk.sequence != len(chunks) or chunk.sample_rate != 24000 or chunk.channels != 1:
                raise RuntimeError('Invalid chunk sequence or format')
            chunks.append(chunk.pcm_s16le)
        pcm = b''.join(chunks)
        with wave.open(str(args.reference), 'rb') as source:
            reference = source.readframes(source.getnframes())
        result = {'first_pcm_before_remaining_text': True,
                  'client_first_pcm_ms': elapsed, 'server_first_pcm_ms': first.first_pcm_ms,
                  'chunks': len(chunks), 'pcm_bytes': len(pcm),
                  'reference_pcm_exact': pcm == reference,
                  'sha256': hashlib.sha256(pcm).hexdigest()}
        print(json.dumps(result, indent=2))
        if pcm != reference:
            raise RuntimeError('Reference PCM mismatch')


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('--address', default='127.0.0.1:8766')
    parser.add_argument('--reference', type=Path, required=True)
    asyncio.run(check(parser.parse_args()))
