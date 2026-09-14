"""Real loopback transport tests with a deterministic, lightweight voice."""
import asyncio
import unittest
import grpc
import tts_stream_pb2 as pb
import tts_stream_pb2_grpc as rpc
from voice_grpc import make_server


class FakeVoice:
    def chunks_for_text(self, parts, max_frames, *, incremental):
        for text in parts:
            if not text:
                raise ValueError('Empty text')
            yield text.encode(), {}


class TransportTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.server, port = await make_server(FakeVoice(), 0, 64)
        self.channel = grpc.aio.insecure_channel(f'127.0.0.1:{port}')
        self.stub = rpc.SpeechStub(self.channel)

    async def asyncTearDown(self):
        await self.channel.close()
        await self.server.stop(0)

    async def test_audio_before_remaining_text(self):
        call = self.stub.Synthesize(timeout=3)
        await call.write(pb.TextPart(text='first'))
        first = await call.read()
        self.assertEqual(first.pcm_s16le, b'first')
        self.assertEqual(first.sample_rate, 24000)
        await call.write(pb.TextPart(text='second'))
        await call.done_writing()
        second = await call.read()
        self.assertEqual(second.pcm_s16le, b'second')
        self.assertEqual(second.sequence, 1)
        self.assertIs(await call.read(), grpc.aio.EOF)

    async def test_cancel_waiting_then_reuse(self):
        call = self.stub.Synthesize(timeout=3)
        await call.write(pb.TextPart(text='first'))
        await call.read()
        busy = self.stub.Synthesize(timeout=3)
        with self.assertRaises(grpc.aio.AioRpcError) as error:
            await busy.read()
        self.assertEqual(error.exception.code(), grpc.StatusCode.RESOURCE_EXHAUSTED)
        call.cancel()
        await call.code()
        for _ in range(100):
            next_call = self.stub.Synthesize(timeout=3)
            await next_call.write(pb.TextPart(text='next'))
            await next_call.done_writing()
            try:
                output = await next_call.read()
                break
            except grpc.aio.AioRpcError as error:
                if error.code() != grpc.StatusCode.RESOURCE_EXHAUSTED:
                    raise
                await asyncio.sleep(.01)
        else:
            self.fail('Cancelled worker did not release voice')
        self.assertEqual(output.pcm_s16le, b'next')
        self.assertIs(await next_call.read(), grpc.aio.EOF)

    async def test_invalid_input(self):
        call = self.stub.Synthesize(timeout=3)
        await call.write(pb.TextPart(text=''))
        await call.done_writing()
        with self.assertRaises(grpc.aio.AioRpcError) as error:
            await call.read()
        self.assertEqual(error.exception.code(), grpc.StatusCode.INVALID_ARGUMENT)


if __name__ == '__main__':
    unittest.main()
