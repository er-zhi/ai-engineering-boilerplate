"""Assemble the runnable HF folder without copying model bytes on one filesystem."""
import argparse
import hashlib
import json
import os
import shutil
from pathlib import Path


def copytree(source,target):
    def link_or_copy(src,dst):
        try: os.link(src,dst)
        except OSError: shutil.copy2(src,dst)
        return dst
    shutil.copytree(source,target,copy_function=link_or_copy)


def sha256(path):
    with path.open('rb') as source:
        return hashlib.file_digest(source,'sha256').hexdigest()


def main():
    parser=argparse.ArgumentParser()
    parser.add_argument('output',type=Path)
    parser.add_argument('--models',type=Path,default=Path('models'))
    parser.add_argument('--sample',type=Path,required=True)
    args=parser.parse_args()
    root=Path(__file__).resolve().parent
    if args.output.exists(): parser.error('Output exists')
    args.output.mkdir(parents=True)
    files=['serve.py','client.py','voice_stream.py','voice_grpc.py','runtime_frontend.py',
           'text_parts.py','tts_stream.proto','tts_stream_pb2.py','tts_stream_pb2_grpc.py',
           'ane_gate.swift','requirements-runtime.txt']
    for name in files: shutil.copy2(root/name,args.output/name)
    shutil.copy2(root/'MODEL_CARD.md',args.output/'README.md')
    shutil.copy2(root/'LICENSE',args.output/'LICENSE')
    shutil.copy2(root/'HF_GITATTRIBUTES',args.output/'.gitattributes')
    copytree(args.models/'qwen06-runtime-assets-hybrid',args.output/'frontend')
    destination=args.output/'models'; destination.mkdir()
    selected={
      args.models/'qwen06-stateful-fp16'/'qwen06_cached_block0.mlpackage':destination/'talker'/'qwen06_cached_block0.mlpackage',
      args.models/'qwen06-full-fp16'/'qwen06_prefill_block0.mlpackage':destination/'prefill'/'qwen06_prefill_block0.mlpackage',
      args.models/'qwen06-outlier256-w8-safe-down.mlpackage':destination/'qwen06-outlier256-w8-safe-down.mlpackage',
      args.models/'streaming-decoder-explicit-noslice-fp16.mlpackage':destination/'streaming-decoder-explicit-noslice-fp16.mlpackage',
      args.models/'qwen06-text-projection32.mlpackage':destination/'qwen06-text-projection32.mlpackage',
      args.models/'qwen06-long512-fp16.mlpackage':destination/'qwen06-long512-fp16.mlpackage'}
    for source,target in selected.items():
        target.parent.mkdir(parents=True,exist_ok=True); copytree(source,target)
    samples=args.output/'samples'; samples.mkdir(); shutil.copy2(args.sample,samples/'serena.wav')
    manifest={str(path.relative_to(args.output)):{'bytes':path.stat().st_size,'sha256':sha256(path)}
              for path in args.output.rglob('*') if path.is_file()}
    (args.output/'release-manifest.json').write_text(json.dumps({
        'files':manifest,'total_bytes':sum(item['bytes'] for item in manifest.values())},indent=2)+'\n')
    print(json.dumps({'files':len(manifest),'bytes':sum(item['bytes'] for item in manifest.values()),'output':str(args.output)}))


if __name__=='__main__': main()
