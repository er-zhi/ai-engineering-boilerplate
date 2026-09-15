import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from qwen3_tts_ane import Qwen3TTSANE
from shared_weights import WEIGHT_RELATIVE_PATH

COMPONENTS = {
    "talker": "models/talker/qwen06_cached_block0.mlpackage",
    "prefill": "models/prefill/qwen06_prefill_block0.mlpackage",
    "long_talker": "models/qwen06-long512-fp16.mlpackage",
    "predictor": "models/predictor.mlpackage",
    "decoder": "models/decoder.mlpackage",
    "text_projection": "models/text-projection.mlpackage",
    "frontend": "frontend",
}


class ModelConfigTests(unittest.TestCase):
    def make_bundle(self, root, components=COMPONENTS):
        (root / "model-config.json").write_text(
            json.dumps({"schema_version": 1, "components": components})
        )
        weight = root / components["talker"] / WEIGHT_RELATIVE_PATH
        weight.parent.mkdir(parents=True)
        weight.write_bytes(b"weights")

    @patch("qwen3_tts_ane.platform.machine", return_value="arm64")
    @patch("qwen3_tts_ane.platform.system", return_value="Darwin")
    @patch("qwen3_tts_ane.VoiceStream")
    @patch("qwen3_tts_ane.materialize_shared_package")
    def test_runtime_uses_component_paths_from_config(self, materialize, voice, system, machine):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.make_bundle(root)
            materialize.side_effect = lambda package, weight, cache: (package, "hard-link")
            runtime = Qwen3TTSANE(root=root, cache=root / "cache")
            self.assertEqual(
                runtime.shared_weight_modes,
                {
                    "prefill": "hard-link",
                    "long_talker": "hard-link",
                },
            )
            self.assertEqual(
                voice.call_args.kwargs["prefill_packages"],
                (root / "models/prefill").resolve(),
            )
            self.assertFalse(voice.call_args.kwargs["experimental_prefix_state"])
            self.assertEqual(
                voice.call_args.kwargs["predictor_package"],
                (root / "models/predictor.mlpackage").resolve(),
            )

    @patch("qwen3_tts_ane.platform.machine", return_value="arm64")
    @patch("qwen3_tts_ane.platform.system", return_value="Darwin")
    @patch("qwen3_tts_ane.VoiceStream")
    @patch("qwen3_tts_ane.materialize_shared_package")
    def test_runtime_can_enable_prefix_kv(self, materialize, voice, system, machine):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.make_bundle(root)
            materialize.side_effect = lambda package, weight, cache: (package, "hard-link")
            Qwen3TTSANE(root=root, cache=root / "cache", use_prefix_kv=True)
            self.assertIsNone(voice.call_args.kwargs["prefill_packages"])
            self.assertTrue(voice.call_args.kwargs["experimental_prefix_state"])

    @patch("qwen3_tts_ane.platform.machine", return_value="arm64")
    @patch("qwen3_tts_ane.platform.system", return_value="Darwin")
    def test_runtime_rejects_component_outside_bundle(self, system, machine):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            components = dict(COMPONENTS)
            components["decoder"] = "../decoder.mlpackage"
            self.make_bundle(root, components)
            with self.assertRaisesRegex(RuntimeError, "escapes"):
                Qwen3TTSANE(root=root, cache=root / "cache")


if __name__ == "__main__":
    unittest.main()
