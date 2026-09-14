import unittest
import tempfile
import threading
from pathlib import Path
import numpy as np
import torch
from transformers import RepetitionPenaltyLogitsProcessor
from voice_stream import VoiceStream, penalize_repetitions, decoder_window, compiled_cache_path


class SamplingTests(unittest.TestCase):
    def test_prefix_state_rejects_overlap_and_releases_on_close(self):
        voice = VoiceStream.__new__(VoiceStream)
        voice.prefix_states = [object()]
        voice.prefix_lock = threading.Lock()
        prefix = np.zeros((1,10,1),np.float32)
        voice.prepared = [{'inputs_embeds':prefix}]
        voice.prefix_identity = prefix[:,:9].tobytes()
        voice._chunks = lambda _: iter([('first',{}),('second',{})])
        stream = voice.chunks()
        self.assertEqual(next(stream)[0],'first')
        with self.assertRaisesRegex(RuntimeError,'already in use'):
            next(voice.chunks())
        stream.close()
        self.assertFalse(voice.prefix_lock.locked())
        self.assertEqual(len(list(voice.chunks())),2)
        prefix[0,0,0] = 1
        with self.assertRaisesRegex(RuntimeError,'identity changed'):
            next(voice.chunks())
        self.assertFalse(voice.prefix_lock.locked())

    def test_matches_transformers(self):
        scores = np.array([-3.,-1.,0.,1.,4.],np.float32)
        for history in [[],[0],[3],[0,0,2,3,4]]:
            for penalty in [0.9,1.,1.05,2.]:
                expected = RepetitionPenaltyLogitsProcessor(penalty)(
                    torch.tensor([history],dtype=torch.long),torch.from_numpy(scores.copy()).unsqueeze(0))
                actual = penalize_repetitions(scores,history,penalty)
                np.testing.assert_array_equal(actual,expected.numpy()[0])
        np.testing.assert_array_equal(scores,[-3.,-1.,0.,1.,4.])

    def test_rejects_invalid_penalties(self):
        for penalty in [0.,-1.,float('nan'),float('inf')]:
            with self.assertRaises(ValueError):
                penalize_repetitions(np.zeros(5,np.float32),[],penalty)

    def test_decoder_window_boundaries(self):
        for frame in range(118):
            width,index = decoder_window(frame)
            self.assertIn(width,range(4,33,4))
            self.assertIn(index,range(4))
            self.assertEqual(width-4+index,min(frame,31))
        with self.assertRaises(ValueError):
            decoder_window(-1)

    def test_model_cache_tracks_content(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            package = root/'test.mlpackage'
            package.mkdir()
            weights = package/'weights.bin'
            weights.write_bytes(b'first')
            first = compiled_cache_path(package,root)
            self.assertEqual(first,compiled_cache_path(package,root))
            weights.write_bytes(b'other')
            self.assertNotEqual(first,compiled_cache_path(package,root))

    def test_calibration_preserves_all_sample_extrema(self):
        from quantize_qwen_activations import update_range
        for samples in [[[-10,20],[-1,2]],[[-1,2],[-10,20]]]:
            stats = {}
            for sample in samples:
                update_range(stats,'activation',np.array(sample,np.float32))
            self.assertEqual(stats['activation'],{'rmin':-10.,'rmax':20.})

    def test_fp32_calibration_range_preserves_outliers(self):
        from quantize_qwen_activations import fp32_calibration_range
        values = fp32_calibration_range({'x':{'rmin':-2.,'rmax':176265.359375}},'x')
        self.assertEqual(values.dtype,np.float32)
        self.assertTrue(np.isfinite(values).all())
        self.assertEqual(float(values[1]),176265.359375)
        for low,high in [(2.,1.),(0.,float('inf')),(float('nan'),1.)]:
            with self.assertRaises(ValueError):
                fp32_calibration_range({'x':{'rmin':low,'rmax':high}},'x')


if __name__ == '__main__':
    unittest.main()
