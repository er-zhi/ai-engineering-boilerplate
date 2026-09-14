import unittest
import numpy as np
import torch
from text_projection_runtime import ANETextProjection


class ProjectionTests(unittest.TestCase):
    def test_lengths_preserve_order_and_drop_padding(self):
        class Model:
            def predict(self, inputs):
                return {'projected': inputs['embeddings']*2+1}
        layer = ANETextProjection(Model(), 3, 32, 3)
        for length in [0, 1, 31, 32, 33, 64, 129]:
            values = torch.arange(length*3, dtype=torch.float32).reshape(1, length, 3)
            torch.testing.assert_close(layer(values), values*2+1)

    def test_rejects_invalid_inputs_and_outputs(self):
        class Model:
            def predict(self, inputs):
                return {'projected': np.full((1, 3, 1, 32), np.nan, np.float32)}
        layer = ANETextProjection(Model(), 3, 32, 3)
        for values in [torch.zeros(2, 1, 3), torch.zeros(1, 2, 4),
                       torch.full((1, 1, 3), float('inf')), torch.zeros(1, 1, 3)]:
            with self.assertRaises(ValueError):
                layer(values)


if __name__ == '__main__':
    unittest.main()
