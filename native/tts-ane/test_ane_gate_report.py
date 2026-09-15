"""Strict admission parser tests; no model weights are loaded."""

import json
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

from voice_stream import _FILE_DIGEST_CACHE, admit_ane_package, compiled_cache_path, gate_report


class GateReportTests(unittest.TestCase):
    def test_hardlinked_weights_are_hashed_once_per_process(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            cache = root / "cache"
            cache.mkdir()
            left = root / "left.mlpackage"
            right = root / "right.mlpackage"
            for package in (left, right):
                (package / "weights").mkdir(parents=True)
                (package / "model").write_bytes(package.name.encode())
            (left / "weights/weight.bin").write_bytes(b"shared")
            (right / "weights/weight.bin").hardlink_to(left / "weights/weight.bin")
            seen = []
            original = __import__("hashlib").file_digest

            def record(source, algorithm):
                seen.append(Path(source.name).stat().st_ino)
                return original(source, algorithm)

            _FILE_DIGEST_CACHE.clear()
            with patch("voice_stream.hashlib.file_digest", side_effect=record):
                compiled_cache_path(left, cache)
                compiled_cache_path(right, cache)
            shared_inode = (left / "weights/weight.bin").stat().st_ino
            self.assertEqual(seen.count(shared_inode), 1)

    def test_gate_free_runtime_compiles_without_swift_inspection(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            target = root / "candidate.mlmodelc"
            with (
                patch("voice_stream.compiled_cache_path", return_value=target),
                patch(
                    "voice_stream.ct.models.utils.compile_model",
                    side_effect=lambda *a, **k: target.mkdir(),
                ) as compile_model,
                patch("voice_stream.subprocess.run") as inspect,
            ):
                first = admit_ane_package(root / "source.mlpackage", None, root)
                second = admit_ane_package(root / "source.mlpackage", None, root)
            self.assertEqual(first["status"], "COMPILED")
            self.assertFalse(first["ane_admission_checked"])
            self.assertEqual(second["compiled_model"], str(target))
            compile_model.assert_called_once()
            inspect.assert_not_called()

    def test_loader_preserves_native_failure_reason(self):
        from benchmark_fused_predictor import load

        result = SimpleNamespace(returncode=1, stderr="native compiler failure detail", stdout="")
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            target = root / "candidate.mlmodelc"
            target.mkdir()
            target.with_name(target.name + ".rejected").mkdir()
            with (
                patch("voice_stream.compiled_cache_path", return_value=target),
                patch("voice_stream.subprocess.run", return_value=result),
                patch("voice_stream.ct.models.utils.compile_model") as compile_model,
            ):
                with self.assertRaisesRegex(RuntimeError, "native compiler failure detail"):
                    load(root / "source.mlpackage", root, root / "gate")
                compile_model.assert_not_called()

    def test_rebuild_preserves_rejected_artifact_and_requires_readmission(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            target = root / "candidate.mlmodelc"
            target.mkdir()
            (target / "sentinel").write_text("original")
            rejected = target.with_name(target.name + ".rejected")
            valid = dict(self.report, compiled_model=str(target))
            results = [
                SimpleNamespace(returncode=1, stderr="unsupported structure", stdout=""),
                SimpleNamespace(returncode=0, stderr="", stdout=json.dumps(valid)),
            ]
            with (
                patch("voice_stream.compiled_cache_path", return_value=target),
                patch("voice_stream.subprocess.run", side_effect=results) as inspect,
                patch(
                    "voice_stream.ct.models.utils.compile_model",
                    side_effect=lambda *a, **k: target.mkdir(),
                ) as compile_model,
            ):
                report = admit_ane_package(root / "source.mlpackage", root / "gate", root)
            self.assertEqual((rejected / "sentinel").read_text(), "original")
            self.assertEqual(report["rebuilt_after_rejection"], str(rejected))
            self.assertEqual(inspect.call_count, 2)
            self.assertEqual(compile_model.call_count, 1)

    def test_new_invalid_compilation_is_not_retried(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            target = root / "candidate.mlmodelc"
            failed = SimpleNamespace(returncode=0, stderr="", stdout="missing admission")
            with (
                patch("voice_stream.compiled_cache_path", return_value=target),
                patch("voice_stream.subprocess.run", return_value=failed),
                patch(
                    "voice_stream.ct.models.utils.compile_model",
                    side_effect=lambda *a, **k: target.mkdir(),
                ) as compile_model,
                self.assertRaisesRegex(RuntimeError, "no further automatic rebuild"),
            ):
                admit_ane_package(root / "source.mlpackage", root / "gate", root)
            self.assertEqual(compile_model.call_count, 1)
            self.assertFalse(target.with_name(target.name + ".rejected").exists())

    def test_valid_admission_is_cached_for_same_machine_and_gate(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            target = root / "candidate.mlmodelc"
            target.mkdir()
            gate = root / "gate"
            gate.write_text("gate")
            valid = dict(self.report, compiled_model=str(target))
            result = SimpleNamespace(returncode=0, stderr="", stdout=json.dumps(valid))
            machine = SimpleNamespace(returncode=0, stdout="Mac16,12\n")
            with (
                patch("voice_stream.compiled_cache_path", return_value=target),
                patch("voice_stream.platform.platform", return_value="test-platform"),
                patch("voice_stream.subprocess.run", side_effect=[machine, result, machine]) as run,
            ):
                first = admit_ane_package(root / "source.mlpackage", gate, root)
                second = admit_ane_package(root / "source.mlpackage", gate, root)
            self.assertNotIn("admission_cached", first)
            self.assertTrue(second["admission_cached"])
            self.assertEqual(run.call_count, 3)

    def setUp(self):
        self.report = dict(
            status="PASS",
            compiled_model=str(Path(tempfile.gettempdir()) / "test.mlmodelc"),
            ane_operation_ratio=1,
            cpu_preferred_operations=0,
            gpu_preferred_operations=0,
        )

    def test_valid(self):
        self.assertEqual(gate_report(json.dumps(self.report)), self.report)

    def test_native_diagnostic_prefix(self):
        self.assertEqual(gate_report("Native diagnostic.\n" + json.dumps(self.report)), self.report)

    def test_missing_report(self):
        with self.assertRaises(RuntimeError):
            gate_report("Native diagnostic without admission evidence")

    def test_cpu_fallback(self):
        self.report["cpu_preferred_operations"] = 1
        with self.assertRaises(RuntimeError):
            gate_report(json.dumps(self.report))

    def test_ambiguous_report(self):
        with self.assertRaises(RuntimeError):
            gate_report(json.dumps(self.report) * 2)


if __name__ == "__main__":
    unittest.main()
