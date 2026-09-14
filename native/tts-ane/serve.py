"""Ready-to-run Serena English gRPC server for the packaged ANE release."""
import argparse
import asyncio
import platform
import subprocess
from pathlib import Path
from voice_grpc import serve_grpc
from voice_stream import VoiceStream


def build_gate(root,cache):
    gate=cache/'ane_gate'
    source=root/'ane_gate.swift'
    if not gate.exists() or gate.stat().st_mtime < source.stat().st_mtime:
        cache.mkdir(parents=True,exist_ok=True)
        subprocess.run(['xcrun','swiftc','-parse-as-library',str(source),'-O','-o',str(gate)],check=True)
    return gate


async def run(args):
    root=Path(__file__).resolve().parent
    cache=args.cache.expanduser().resolve()
    gate=build_gate(root,cache)
    models=root/'models'
    voice=VoiceStream(root/'frontend',models/'talker',gate,
        "I'm ready.",'',compiled_dir=cache/'compiled',
        predictor_package=models/'qwen06-outlier256-w8-safe-down.mlpackage',
        prefill_packages=models/'prefill',speaker='Serena',model_prefix='qwen06',block_count=1,
        decoder_package=models/'streaming-decoder-explicit-noslice-fp16.mlpackage',experimental_history_decoder=True,
        text_projection_package=models/'qwen06-text-projection32.mlpackage',
        long_talker_package=models/'qwen06-long512-fp16.mlpackage',frontend_assets=root/'frontend')
    await serve_grpc(voice,args.port,args.max_frames)


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--port',type=int,default=8766)
    parser.add_argument('--max-frames',type=int,default=502,choices=range(1,503),metavar='1..502')
    parser.add_argument('--cache',type=Path,default=Path('~/Library/Caches/Qwen3TTSANE'))
    args=parser.parse_args()
    if platform.system()!='Darwin' or platform.machine()!='arm64':
        parser.error('This release requires Apple Silicon macOS')
    if not 1<=args.port<=65535:
        parser.error('port must be 1..65535')
    asyncio.run(run(args))


if __name__=='__main__': main()
