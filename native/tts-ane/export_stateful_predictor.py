"""Candidate using Apple's documented StateType API; not an admitted release.

https://apple.github.io/coremltools/docs-guides/source/stateful-models.html
Uses the pinned Qwen predictor equations from export_qwen_predictor.
"""
import argparse
from pathlib import Path

import coremltools as ct
import numpy as np
import torch
from qwen_tts import Qwen3TTSModel
from export_qwen_predictor import Predictor
from export_voice_cached_blocks import CachedBlock


class StatefulPredictor(torch.nn.Module):
    def __init__(self, predictor, talker=False, capacity=None):
        super().__init__()
        self.step = CachedBlock(predictor,0,predictor.config.num_hidden_layers) if talker else Predictor(predictor)
        cfg = predictor.config
        capacity = capacity if capacity is not None else (128 if talker else 16)
        if capacity not in (16,32,64,128):
            raise ValueError('Unsupported cache capacity')
        shape = (cfg.num_hidden_layers, cfg.num_key_value_heads, capacity, cfg.head_dim)
        self.register_buffer('key_cache', torch.zeros(shape))
        self.register_buffer('value_cache', torch.zeros(shape))

    def forward(self, embeddings, write_mask, attention_mask, cosine, sine):
        logits, hidden, keys, values = self.step(
            embeddings, self.key_cache, self.value_cache,
            write_mask, attention_mask, cosine, sine)
        self.key_cache[:] = keys
        self.value_cache[:] = values
        return logits, hidden


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('source', type=Path)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--talker',action='store_true',help='Export the full talker with a 128-position state cache')
    parser.add_argument('--capacity',type=int,choices=[16,32,64,128],help='Experimental short talker cache; runtime migration must be validated separately')
    args = parser.parse_args()
    if args.output.exists():
        parser.error('output already exists; never overwrite a candidate')
    if args.capacity is not None and not args.talker:
        parser.error('--capacity is currently a talker-only experiment')
    torch.set_num_threads(4)
    model = Qwen3TTSModel.from_pretrained(str(args.source), dtype=torch.float32,
        device_map='cpu', local_files_only=True, attn_implementation='eager')
    predictor = (model.model.talker if args.talker else model.model.talker.code_predictor).eval()
    capacity = args.capacity if args.capacity is not None else (128 if args.talker else 16)
    wrapper = StatefulPredictor(predictor,args.talker,capacity).eval()
    cfg = predictor.config
    positions = torch.zeros((3,1,1) if args.talker else (1,1),dtype=torch.long)
    cos, sin = predictor.model.rotary_emb(torch.zeros(1,1,cfg.hidden_size), positions)
    if args.talker:
        cos,sin = cos[0],sin[0]
    write = torch.zeros(1,1,capacity,1)
    write[:,:,0,:] = 1
    mask = torch.full((1,1,1,capacity), float('-inf'))
    mask[:,:,:,0] = 0
    sample = (torch.zeros(1,1,model.model.talker.config.hidden_size),write,mask,
        cos.unsqueeze(1),sin.unsqueeze(1))
    with torch.inference_mode():
        traced = torch.jit.trace(wrapper,sample,strict=False,check_trace=False)
    converted = ct.convert(traced,convert_to='mlprogram',
        inputs=[ct.TensorType(name=n,shape=x.shape,dtype=np.float32) for n,x in zip(
            ['embeddings','write_mask','attention_mask','cosine','sine'],sample)],
        outputs=[ct.TensorType(name=n) for n in ['logits','hidden']],
        states=[ct.StateType(name=n,wrapped_type=ct.TensorType(shape=wrapper.key_cache.shape,
            dtype=np.float16)) for n in ['key_cache','value_cache']],
        minimum_deployment_target=ct.target.macOS15,compute_precision=ct.precision.FLOAT16,
        skip_model_load=True)
    args.output.parent.mkdir(parents=True,exist_ok=True)
    converted.save(args.output)
    print(f'Saved candidate: {args.output}',flush=True)


if __name__ == '__main__':
    main()
