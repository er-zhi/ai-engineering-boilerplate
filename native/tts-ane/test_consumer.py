import json
import tempfile
import unittest
from pathlib import Path

from download_model import read_source


class ModelSourceTests(unittest.TestCase):
    def test_pinned_source_is_complete(self):
        source = read_source()
        self.assertEqual(source["repo_id"], "erjigit17/Qwen3-TTS-0.6B-ANE")
        self.assertRegex(source["revision"], r"^[0-9a-f]{40}$")
        self.assertEqual(source["source_repository"], "https://github.com/er-zhi/qwen3-tts-ane")

    def test_missing_fields_are_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "source.json"
            path.write_text(json.dumps({"repo_id": "example/model"}))
            with self.assertRaisesRegex(ValueError, "missing fields"):
                read_source(path)


if __name__ == "__main__":
    unittest.main()
