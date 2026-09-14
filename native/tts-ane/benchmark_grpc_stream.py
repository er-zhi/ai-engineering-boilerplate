"""Measure real loopback gRPC first-PCM latency and sustained streaming rate."""
import argparse
import asyncio
import json
import statistics
import time
import grpc
import tts_stream_pb2 as pb
import tts_stream_pb2_grpc as rpc


def percentile(values, fraction):
    ordered = sorted(values)
    return ordered[min(len(ordered)-1, int(len(ordered)*fraction))]


async def first_pcm(stub, text, timeout):
    call = stub.Synthesize(timeout=timeout)
    started = time.perf_counter()
    await call.write(pb.TextPart(text=text))
    chunk = await call.read()
    elapsed = (time.perf_counter()-started)*1000
    if chunk is grpc.aio.EOF or len(chunk.pcm_s16le) != 3840:
        raise RuntimeError('Expected one 80 ms PCM chunk')
    call.cancel()
    await call.code()
    return elapsed,chunk.first_pcm_ms


async def complete(stub, text, timeout):
    async def requests():
        yield pb.TextPart(text=text[:max(1,len(text)//2)])
        yield pb.TextPart(text=text[max(1,len(text)//2):])
    started = time.perf_counter()
    chunks = []
    async for chunk in stub.Synthesize(requests(),timeout=timeout):
        if chunk.sequence != len(chunks):
            raise RuntimeError('Out-of-order chunk')
        chunks.append(chunk.pcm_s16le)
    wall = time.perf_counter()-started
    audio = sum(map(len,chunks))/2/24000
    return {'wall_seconds':wall,'audio_seconds':audio,'realtime_multiple':audio/wall,
            'chunks':len(chunks),'pcm_bytes':sum(map(len,chunks))}


async def when_available(operation):
    for _ in range(200):
        try:
            return await operation()
        except grpc.aio.AioRpcError as error:
            if error.code()!=grpc.StatusCode.RESOURCE_EXHAUSTED:
                raise
            await asyncio.sleep(.01)
    raise RuntimeError('Cancelled synthesis did not release the voice')


async def benchmark(args):
    async with grpc.aio.insecure_channel(args.address) as channel:
        await channel.channel_ready()
        stub = rpc.SpeechStub(channel)
        client=[];server=[]
        for _ in range(args.first_pcm_trials):
            c,s = await when_available(lambda: first_pcm(stub,args.text,args.timeout))
            client.append(c);server.append(s)
        runs=[await when_available(lambda: complete(stub,args.text,args.timeout))
              for _ in range(args.complete_trials)]
    result={'scope':'warm loopback gRPC; new text included; model load/compile excluded',
            'first_pcm_trials':len(client),
            'client_first_pcm_ms':{'p50':statistics.median(client),'p95':percentile(client,.95),'max':max(client)},
            'server_first_pcm_ms':{'p50':statistics.median(server),'p95':percentile(server,.95),'max':max(server)},
            'complete_runs':runs,
            'realtime_multiple':{'p50':statistics.median(x['realtime_multiple'] for x in runs),
                                 'min':min(x['realtime_multiple'] for x in runs)}}
    print(json.dumps(result,indent=2))
    args.output.write_text(json.dumps(result,indent=2)+'\n')
    if result['realtime_multiple']['min'] < 1:
        raise RuntimeError('Generation fell behind real time')


if __name__=='__main__':
    parser=argparse.ArgumentParser()
    parser.add_argument('--address',default='127.0.0.1:8766')
    parser.add_argument('--text',default="I'm sorry about the charge. I'll fix it for you.")
    parser.add_argument('--first-pcm-trials',type=int,default=30)
    parser.add_argument('--complete-trials',type=int,default=3)
    parser.add_argument('--timeout',type=float,default=60)
    parser.add_argument('--output',type=__import__('pathlib').Path,required=True)
    asyncio.run(benchmark(parser.parse_args()))
