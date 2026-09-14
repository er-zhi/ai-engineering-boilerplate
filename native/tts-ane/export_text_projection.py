"""Export learned text projection only; tokenizer/embedding lookup remain host-side.

BC1S layout: https://machinelearning.apple.com/research/neural-engine-transformers
"""
import argparse
import json
from pathlib import Path
import time

import coremltools as ct
import numpy as np
import torch
from qwen_tts import Qwen3TTSModel

from benchmark_fused_predictor import load


class TextProjection(torch.nn.Module):
    def __init__(self, source):
        super().__init__()
        self.source = source

    def forward(self, embeddings):
        def linear(layer, x):
            return torch.nn.functional.conv2d(x, layer.weight[:,:,None,None], layer.bias)
        return linear(self.source.linear_fc2,
            self.source.act_fn(linear(self.source.linear_fc1, embeddings)))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('source',type=Path)
    parser.add_argument('--output',type=Path,required=True)
    parser.add_argument('--report',type=Path,required=True)
    parser.add_argument('--length',type=int,default=32,choices=[1,16,32,64,128])
    parser.add_argument('--gate',type=Path,required=True)
    args = parser.parse_args()
    if args.output.exists() or args.report.exists():
        parser.error('Preserve earlier models and reports')
    torch.set_num_threads(4)
    tts = Qwen3TTSModel.from_pretrained(str(args.source),dtype=torch.float32,
        device_map='cpu',local_files_only=True,attn_implementation='eager')
    source = tts.model.talker.text_projection.eval()
    table = tts.model.talker.get_text_embeddings()
    wrapper = TextProjection(source).eval()
    torch.manual_seed(42)
    # Actual model embeddings, deterministic token IDs; not perceptual validation.
    ids = torch.randint(0,table.num_embeddings,(1,args.length))
    with torch.inference_mode():
        embedded = table(ids)
        expected = source(embedded).transpose(1,2).unsqueeze(2)
        sample = embedded.transpose(1,2).unsqueeze(2)
        torch.testing.assert_close(wrapper(sample),expected,atol=1e-5,rtol=1e-4)
        traced = torch.jit.trace(wrapper,(sample,),check_trace=False)
    model = ct.convert(traced,inputs=[ct.TensorType(name='embeddings',shape=sample.shape)],
        outputs=[ct.TensorType(name='projected')],minimum_deployment_target=ct.target.macOS15,
        compute_precision=ct.precision.FLOAT16,skip_model_load=True)
    args.output.parent.mkdir(parents=True,exist_ok=True)
    model.save(str(args.output))
    compiled,admission = load(args.output,args.output.parent/'compiled-cache',args.gate)
    inputs = {'embeddings':sample.numpy()}
    observed = compiled.predict(inputs)['projected']
    if not np.isfinite(observed).all():
        raise ValueError('Non-finite projected text')
    error = observed-expected.numpy()
    times = []
    for i in range(105):
        start = time.perf_counter()
        compiled.predict(inputs)
        elapsed = (time.perf_counter()-start)*1000
        if i>=5:
            times.append(elapsed)
    report = {'scope':'Learned text projection only; no tokenizer, embedding lookup or audio generation',
        'admission':admission,'tokens':args.length,'source_fp32_peak_error':float(np.abs(error).max()),
        'source_fp32_rms_error':float(np.sqrt(np.mean(error**2))),
        'p50_ms':float(np.percentile(times,50)),'p95_ms':float(np.percentile(times,95)),
        'raw_ms':times,'runtime_integrated':False,'quality_validated':False}
    args.report.parent.mkdir(parents=True,exist_ok=True)
    args.report.write_text(json.dumps(report,indent=2)+'\n')
    print(json.dumps({k:v for k,v in report.items() if k not in ['admission','raw_ms']}),flush=True)


if __name__ == '__main__':
    main()
