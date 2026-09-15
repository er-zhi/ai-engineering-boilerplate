import unittest

from qwen3_tts_ane import Qwen3TTSANE


class FakeVoice:
    def chunks_for_text(self, text, max_frames):
        yield b"one", {"text": text, "max_frames": max_frames}
        yield b"two", {"text": text, "max_frames": max_frames}


class ModelApiTests(unittest.TestCase):
    def setUp(self):
        self.runtime = Qwen3TTSANE.__new__(Qwen3TTSANE)
        self.runtime._voice = FakeVoice()

    def test_stream_exposes_ordered_pcm_chunks(self):
        chunks = list(self.runtime.stream("hello", 2))
        self.assertEqual([chunk.sequence for chunk in chunks], [0, 1])
        self.assertEqual([chunk.pcm_s16le for chunk in chunks], [b"one", b"two"])
        self.assertEqual(chunks[0].sample_rate, 24000)

    def test_synthesize_joins_streamed_pcm(self):
        self.assertEqual(self.runtime.synthesize("hello", 2), b"onetwo")

    def test_stream_rejects_invalid_boundaries(self):
        for text in ["", " ", "x" * 32769, None]:
            with self.subTest(text=type(text).__name__), self.assertRaises(ValueError):
                list(self.runtime.stream(text, 2))
        for frames in [0, 503, True, 1.5]:
            with self.subTest(frames=frames), self.assertRaises(ValueError):
                list(self.runtime.stream("hello", frames))


if __name__ == "__main__":
    unittest.main()
