"""Dependency-preservation checks for the diagnostic Q/DQ bypass."""
import unittest
from types import SimpleNamespace as Node

from select_activation_boundaries import exclude_linear_outputs


def operation(kind, name, **inputs):
    return Node(type=kind, outputs=[Node(name=name)], inputs={
        key: Node(arguments=[Node(name=value)]) for key, value in inputs.items()})


def fixture():
    return Node(outputs=['result'], operations=[
        operation('mul', 'activation'),
        operation('quantize', 'qi', input='activation'),
        operation('dequantize', 'di', input='qi'),
        operation('linear', 'projection', x='di'),
        operation('quantize', 'qo', input='projection'),
        operation('dequantize', 'do', input='qo'),
        operation('reshape', 'result', x='do')])


class ActivationBoundaryTests(unittest.TestCase):
    def test_only_linear_output_pair_is_bypassed(self):
        block = fixture()
        self.assertEqual(exclude_linear_outputs(block), (1, 1))
        self.assertEqual([o.outputs[0].name for o in block.operations],
                         ['activation', 'qi', 'di', 'projection', 'result'])
        self.assertEqual(block.operations[-1].inputs['x'].arguments[0].name,
                         'projection')
        self.assertEqual(block.operations[-2].inputs['x'].arguments[0].name, 'di')

    def test_public_quantized_output_is_rejected(self):
        block = fixture()
        block.outputs.append('qo')
        with self.assertRaisesRegex(ValueError, 'public'):
            exclude_linear_outputs(block)

    def test_other_quantized_consumer_is_rejected(self):
        block = fixture()
        block.operations.append(operation('cast', 'extra', x='qo'))
        with self.assertRaisesRegex(ValueError, 'another consumer'):
            exclude_linear_outputs(block)


if __name__ == '__main__':
    unittest.main()
