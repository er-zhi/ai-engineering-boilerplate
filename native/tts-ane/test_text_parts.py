"""Incremental token-boundary and producer-lifetime tests without model weights."""
import unittest
from types import SimpleNamespace
import numpy as np
import torch
from text_parts import TextParts


class Frontend:
    device = 'cpu'
    model = SimpleNamespace(talker=SimpleNamespace(
        get_text_embeddings=lambda:lambda ids:ids.unsqueeze(-1).float(),
        text_projection=lambda embeddings:embeddings))

    def _build_assistant_text(self,text):
        return text

    def _tokenize_texts(self,texts):
        return [torch.tensor([[0]*3+list(map(ord,texts[0]))+[0]*5])]


def capture(text):
    tail = np.array([*map(ord,text[1:]),-1],np.float32).reshape(1,-1,1)
    return {'trailing_text_hidden':tail,'tts_pad_embed':np.zeros((1,1,1),np.float32)}


class TextPartsTests(unittest.TestCase):
    def test_split_words_preserve_all_characters_and_eos(self):
        text = "I'm sorry about the charge."
        for width in [1,2,7,len(text)]:
            source = TextParts(Frontend(),(text[i:i+width] for i in range(0,len(text),width)),capture)
            values = [source.next_hidden().item() for _ in range(len(text)+1)]
            self.assertEqual(values,[*map(ord,text[1:]),-1,0])
            self.assertTrue(source.finished)
            self.assertTrue(source.eos_sent)

    def test_first_word_does_not_consume_rest_and_close_releases_producer(self):
        events = []
        def producer():
            try:
                events.append('first')
                yield 'hello '
                events.append('second')
                yield 'world'
            finally:
                events.append('closed')
        source = TextParts(Frontend(),producer(),capture)
        self.assertEqual(events,['first'])
        self.assertFalse(source.finished)
        source.close()
        self.assertEqual(events,['first','closed'])

    def test_invalid_or_unbounded_fragments_fail(self):
        for parts in [[],[42],['']*4097,['a'*32769]]:
            with self.assertRaises(ValueError):
                TextParts(Frontend(),parts,capture)


if __name__ == '__main__':
    unittest.main()
