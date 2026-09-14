"""Export separate FP16-output candidate with the public Core ML conversion API.

Coremltools 9.0 models/utils.py: change_input_output_tensor_type.
Python CompiledMLModel still promotes FP16 inputs; this is not zero-copy IO.
"""
import argparse
from pathlib import Path
import coremltools as ct
from coremltools.proto.FeatureTypes_pb2 import ArrayFeatureType


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('source',type=Path)
    parser.add_argument('output',type=Path)
    args = parser.parse_args()
    if args.output.exists():
        parser.error('Preserve previous model')
    original = ct.models.MLModel(str(args.source),skip_model_load=True)
    candidate = ct.models.utils.change_input_output_tensor_type(original,
        from_type=ArrayFeatureType.FLOAT32,to_type=ArrayFeatureType.FLOAT16)
    candidate.save(str(args.output))
    print('Saved experimental FP16 outputs: '+str(args.output),flush=True)


if __name__ == '__main__':
    main()
