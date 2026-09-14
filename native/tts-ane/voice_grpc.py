"""Loopback bidirectional gRPC; neural work stays off the asyncio thread."""
import asyncio
import threading
import time
import grpc
import tts_stream_pb2 as pb
import tts_stream_pb2_grpc as rpc


class Bridge:
    def __init__(self, loop):
        self.loop = loop
        self.lock = threading.Lock()
        self.stopped = False
        self.pending = None

    def cancel(self):
        with self.lock:
            self.stopped = True
            if self.pending is not None:
                self.pending.cancel()

    def wait(self, coroutine):
        with self.lock:
            if self.stopped:
                coroutine.close()
                raise RuntimeError('Synthesis cancelled')
            future = asyncio.run_coroutine_threadsafe(coroutine, self.loop)
            self.pending = future
        try:
            return future.result(timeout=30)
        finally:
            with self.lock:
                if not future.done():
                    future.cancel()
                self.pending = None


class Speech(rpc.SpeechServicer):
    def __init__(self, voice, max_frames):
        self.voice = voice
        self.max_frames = max_frames
        self.active = asyncio.Lock()

    async def Synthesize(self, requests, context):
        if self.active.locked():
            await context.abort(grpc.StatusCode.RESOURCE_EXHAUSTED, 'Voice is busy')
        async with self.active:
            queue = asyncio.Queue(maxsize=32)
            bridge = Bridge(asyncio.get_running_loop())
            timing = {}

            async def receive():
                count = total = 0
                async for request in requests:
                    count += 1
                    total += len(request.text)
                    if count > 4096 or total > 32768:
                        raise ValueError('Text limit exceeded')
                    timing.setdefault('start', time.perf_counter())
                    await queue.put(request.text)
                await queue.put(None)

            def parts():
                while True:
                    value = bridge.wait(queue.get())
                    if value is None:
                        return
                    yield value

            def synthesize():
                stream = self.voice.chunks_for_text(parts(), self.max_frames, incremental=True)
                try:
                    for index, (pcm, _) in enumerate(stream):
                        elapsed = (time.perf_counter() - timing['start']) * 1000 if index == 0 else 0
                        bridge.wait(context.write(pb.AudioChunk(
                            pcm_s16le=pcm, sequence=index, sample_rate=24000,
                            channels=1, first_pcm_ms=elapsed)))
                finally:
                    stream.close()

            receiver = asyncio.create_task(receive())
            worker = asyncio.create_task(asyncio.to_thread(synthesize))
            try:
                done, _ = await asyncio.wait([receiver, worker], return_when=asyncio.FIRST_COMPLETED)
                if receiver in done:
                    receiver.result()
                await asyncio.shield(worker)
            except ValueError as error:
                await context.abort(grpc.StatusCode.INVALID_ARGUMENT, str(error))
            except TimeoutError:
                await context.abort(grpc.StatusCode.DEADLINE_EXCEEDED, 'Text or output idle for 30 seconds')
            except asyncio.CancelledError:
                raise
            except Exception:
                await context.abort(grpc.StatusCode.INTERNAL, 'Synthesis failed')
            finally:
                bridge.cancel()
                receiver.cancel()
                await asyncio.gather(receiver, worker, return_exceptions=True)


async def make_server(voice, port, max_frames):
    server = grpc.aio.server(options=[('grpc.max_receive_message_length', 131072)])
    rpc.add_SpeechServicer_to_server(Speech(voice, max_frames), server)
    bound = server.add_insecure_port(f'127.0.0.1:{port}')
    await server.start()
    return server, bound


async def serve_grpc(voice, port, max_frames):
    server, bound = await make_server(voice, port, max_frames)
    print(f'Bidirectional text/PCM gRPC on 127.0.0.1:{bound}', flush=True)
    try:
        await server.wait_for_termination()
    finally:
        await server.stop(0)
