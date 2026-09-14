"""Export all residual-code heads with a fixed 16-position predictor cache."""
import argparse
import json
from pathlib import Path
import coremltools as ct
import numpy as np
import torch
from qwen_tts import Qwen3TTSModel
from export_qwen_cached_step import CachedStep


class Heads(torch.nn.Module):
    def __init__(self, heads):
        super().__init__()
        self.heads = heads

    def forward(self, hidden):
        return torch.cat([head(hidden) for head in self.heads], dim=1)


class Predictor(CachedStep):
    def __init__(self, predictor):
        torch.nn.Module.__init__(self)
        self.model = predictor.model
        self.head = Heads(predictor.lm_head)
        self.projection = predictor.small_to_mtp_projection

    def forward(self, embeddings, keys, values, write_mask, attention_mask, cosine, sine):
        return super().forward(self.projection(embeddings),keys,values,write_mask,attention_mask,cosine,sine)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('model',type=Path)
    parser.add_argument('--output',type=Path,required=True)
    args = parser.parse_args()
    torch.set_num_threads(4)
    tts = Qwen3TTSModel.from_pretrained(str(args.model),dtype=torch.float32,
        device_map='cpu',local_files_only=True,attn_implementation='eager')
    predictor = tts.model.talker.code_predictor.eval()
    wrapper = Predictor(predictor).eval()
    cfg = predictor.config
    keys = torch.zeros(cfg.num_hidden_layers,cfg.num_key_value_heads,16,cfg.head_dim)
    values = torch.zeros_like(keys)
    torch.manual_seed(7)
    sequence = torch.randn(1,16,tts.model.talker.config.hidden_size)
    errors = []
    with torch.inference_mode():
        for position in range(16):
            write = torch.zeros(1,1,16,1)
            write[:,:,position,:] = 1
            mask = torch.full((1,1,1,16),float('-inf'))
            mask[...,:position+1] = 0
            cos,sin = predictor.model.rotary_emb(torch.zeros(1,1,cfg.hidden_size),torch.tensor([[position]]))
            inputs = (sequence[:,position:position+1],keys,values,write,mask,cos.unsqueeze(1),sin.unsqueeze(1))
            logits,_,keys,values = wrapper(*inputs)
            hidden = predictor.model(inputs_embeds=predictor.small_to_mtp_projection(sequence[:,:position+1]),use_cache=False).last_hidden_state[:,-1:]
            expected = wrapper.head(hidden)
            torch.testing.assert_close(logits,expected,atol=2e-4,rtol=2e-4)
            errors.append((logits-expected).abs().max().item())
        print(json.dumps({'predictor_steps':16,'max_abs_error':max(errors)}),flush=True)
        traced = torch.jit.trace(wrapper,inputs,strict=False,check_trace=False)
    converted = ct.convert(traced,convert_to='mlprogram',
        inputs=[ct.TensorType(name=n,shape=x.shape,dtype=np.float32) for n,x in zip(
            ['embeddings','keys','values','write_mask','attention_mask','cosine','sine'],inputs)],
        outputs=[ct.TensorType(name=n) for n in ['logits','hidden','next_keys','next_values']],
        compute_precision=ct.precision.FLOAT16,minimum_deployment_target=ct.target.macOS15)
    args.output.parent.mkdir(parents=True,exist_ok=True)
    converted.save(args.output)
    print(f'Saved {args.output}',flush=True)


if __name__=='__main__':
    main()
