"""Strict admission parser tests; no model weights are loaded."""
import json
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch
from voice_stream import gate_report


class GateReportTests(unittest.TestCase):
    def test_loader_preserves_native_failure_reason(self):
        from benchmark_fused_predictor import load
        result = SimpleNamespace(returncode=1,stderr='native compiler failure detail',stdout='')
        with patch('benchmark_fused_predictor.compiled_cache_path',return_value=Path('/tmp/existing.mlmodelc')), \
            patch.object(Path,'mkdir'),patch.object(Path,'exists',return_value=True), \
            patch('benchmark_fused_predictor.subprocess.run',return_value=result):
            with self.assertRaisesRegex(RuntimeError,'native compiler failure detail'):
                load(Path('/tmp/model.mlpackage'),Path('/tmp/cache'),Path('/tmp/gate'))

    def setUp(self):
        self.report = dict(status='PASS',compiled_model='/tmp/test.mlmodelc',
            ane_operation_ratio=1,cpu_preferred_operations=0,gpu_preferred_operations=0)

    def test_valid(self):
        self.assertEqual(gate_report(json.dumps(self.report)),self.report)

    def test_native_diagnostic_prefix(self):
        self.assertEqual(gate_report('Native diagnostic.\n'+json.dumps(self.report)),self.report)

    def test_missing_report(self):
        with self.assertRaises(RuntimeError):
            gate_report('Native diagnostic without admission evidence')

    def test_cpu_fallback(self):
        self.report['cpu_preferred_operations'] = 1
        with self.assertRaises(RuntimeError):
            gate_report(json.dumps(self.report))

    def test_ambiguous_report(self):
        with self.assertRaises(RuntimeError):
            gate_report(json.dumps(self.report)*2)


if __name__=='__main__':
    unittest.main()
