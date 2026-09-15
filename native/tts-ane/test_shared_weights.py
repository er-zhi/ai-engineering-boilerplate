import tempfile
import unittest
from pathlib import Path

from build_hf_release import copy_shared_package, sha256
from shared_weights import WEIGHT_RELATIVE_PATH, materialize_shared_package


def make_package(root, name, weight):
    package = root / f"{name}.mlpackage"
    (package / "Data/com.apple.CoreML/weights").mkdir(parents=True)
    (package / "Data/com.apple.CoreML/model.mlmodel").write_bytes(name.encode())
    (package / "Manifest.json").write_text("{}")
    (package / WEIGHT_RELATIVE_PATH).write_bytes(weight)
    return package


class SharedWeightsTests(unittest.TestCase):
    def test_release_omits_duplicate_and_runtime_materializes_hard_link(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            canonical = make_package(root, "talker", b"shared weights")
            source = make_package(root, "prefill", b"shared weights")
            skeleton = root / "release" / "prefill.mlpackage"
            copy_shared_package(source, skeleton, sha256(canonical / WEIGHT_RELATIVE_PATH))
            self.assertFalse((skeleton / WEIGHT_RELATIVE_PATH).exists())
            package, mode = materialize_shared_package(
                skeleton, canonical / WEIGHT_RELATIVE_PATH, root / "cache"
            )
            self.assertEqual(mode, "hard-link")
            self.assertTrue(
                (package / WEIGHT_RELATIVE_PATH).samefile(canonical / WEIGHT_RELATIVE_PATH)
            )
            self.assertFalse((skeleton / WEIGHT_RELATIVE_PATH).exists())

    def test_release_rejects_different_shared_weight(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            canonical = make_package(root, "talker", b"canonical")
            source = make_package(root, "prefill", b"different")
            with self.assertRaisesRegex(ValueError, "weights differ"):
                copy_shared_package(
                    source, root / "release.mlpackage", sha256(canonical / WEIGHT_RELATIVE_PATH)
                )


if __name__ == "__main__":
    unittest.main()
