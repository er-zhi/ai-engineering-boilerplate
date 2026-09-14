"""Small bidirectional gRPC client that writes the streamed PCM to WAV."""
import argparse
import asyncio
import time
import wave
import grpc
import tts_stream_pb2 as pb
import tts_stream_pb2_grpc as rpc


async def synthesize(args):
    async with grpc.aio.insecure_channel(args.address) as channel:
        await channel.channel_ready()
        call=rpc.SpeechStub(channel).Synthesize(timeout=args.timeout)
        words=args.text.split(' ')
        started=time.perf_counter()
        for index,word in enumerate(words):
            await call.write(pb.TextPart(text=word+(' ' if index<len(words)-1 else '')))
        await call.done_writing()
        chunks=[]
        while True:
            chunk=await call.read()
            if chunk is grpc.aio.EOF: break
            if not chunks:
                print(f'first PCM: {(time.perf_counter()-started)*1000:.1f} ms')
            chunks.append(chunk.pcm_s16le)
    with wave.open(str(args.output),'wb') as wav:
        wav.setnchannels(1); wav.setsampwidth(2); wav.setframerate(24000)
        wav.writeframes(b''.join(chunks))
    print(f'{len(chunks)*.08:.2f} s audio -> {args.output}')


if __name__=='__main__':
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('text')
    parser.add_argument('--address',default='127.0.0.1:8766')
    parser.add_argument('--output',type=__import__('pathlib').Path,default='output.wav')
    parser.add_argument('--timeout',type=float,default=120)
    asyncio.run(synthesize(parser.parse_args()))
