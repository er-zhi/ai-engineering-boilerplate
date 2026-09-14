import tempfile
import unittest
from pathlib import Path
import numpy as np
from runtime_frontend import ArrayEmbedding


class ArrayEmbeddingTests(unittest.TestCase):
    def test_sparse_correction_restores_fp32_rows(self):
        with tempfile.TemporaryDirectory() as directory:
            path=Path(directory)/'table.npy'
            original=np.asarray([[0,2.9802322e-8],[1,-2.9802322e-8]],np.float32)
            np.save(path,original.astype(np.float16))
            np.savez_compressed(path.with_suffix('.corrections.npz'),
                flat_index=np.asarray([1,3],np.int64),value=original.reshape(-1)[[1,3]])
            table=ArrayEmbedding(path)
            np.testing.assert_array_equal(table(np.asarray([[1,0]])),original[[[1,0]]])
            np.testing.assert_array_equal(table[0],original[0])


if __name__=='__main__': unittest.main()
