"""Fixed-capacity talker decode with explicit KV inputs/outputs; no eviction."""
import argparse
import json
from pathlib import Path

import coremltools as ct
import numpy as np
import torch
from qwen_tts import Qwen3TTSModel
from qwen_tts.core.models.modeling_qwen3_tts import rotate_half, repeat_kv


class CachedStep(torch.nn.Module):
    def __init__(self, talker, capacity):
        super().__init__()
        self.model = talker.model
        self.head = talker.codec_head
        self.capacity = capacity

    def forward(self, embeddings, keys, values, write_mask, attention_mask, cosine, sine):
        hidden = embeddings
        next_keys, next_values = [], []
        for index, layer in enumerate(self.model.layers):
            residual = hidden
            normalized = layer.input_layernorm(hidden)
            attn = layer.self_attn
            shape = (1, 1, -1, attn.head_dim)
            q = attn.q_norm(attn.q_proj(normalized).view(shape)).transpose(1, 2)
            k = attn.k_norm(attn.k_proj(normalized).view(shape)).transpose(1, 2)
            v = attn.v_proj(normalized).view(shape).transpose(1, 2)
            q = q * cosine + rotate_half(q) * sine
            k = k * cosine + rotate_half(k) * sine
            k = keys[index:index+1] * (1-write_mask) + k * write_mask
            v = values[index:index+1] * (1-write_mask) + v * write_mask
            next_keys.append(k)
            next_values.append(v)
            scores = torch.matmul(q, repeat_kv(k, attn.num_key_value_groups).transpose(2,3)) * attn.scaling
            probabilities = torch.softmax(scores + attention_mask, dim=-1)
            output = torch.matmul(probabilities, repeat_kv(v, attn.num_key_value_groups))
            hidden = residual + attn.o_proj(output.transpose(1,2).reshape(1,1,-1))
            hidden = hidden + layer.mlp(layer.post_attention_layernorm(hidden))
        hidden = self.model.norm(hidden)
        return self.head(hidden), hidden, torch.cat(next_keys, dim=0), torch.cat(next_values, dim=0)


def controls(model, position, capacity):
    if not 0 <= position < capacity:
        raise ValueError('KV capacity exceeded: start a new session; silent eviction is forbidden')
    mask = torch.zeros(1,1,capacity,1)
    mask[:,:,position,:] = 1
    attention = torch.full((1,1,1,capacity), float('-inf'))
    attention[...,:position+1] = 0
    pos = torch.full((3,1,1), position, dtype=torch.long)
    cos, sin = model.rotary_emb(torch.zeros(1,1,model.config.hidden_size), pos)
    return mask, attention, cos[0].unsqueeze(1), sin[0].unsqueeze(1)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('model', type=Path)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--capacity', type=int, default=64)
    args = parser.parse_args()
    if not 8 <= args.capacity <= 1024:
        parser.error('capacity must be between 8 and 1024')
    torch.set_num_threads(4)
    talker = Qwen3TTSModel.from_pretrained(str(args.model), device_map='cpu',
        dtype=torch.float32, local_files_only=True, attn_implementation='eager').model.talker.eval()
    wrapper = CachedStep(talker,args.capacity).eval()
    config = talker.config
    keys = torch.zeros(config.num_hidden_layers, config.num_key_value_heads, args.capacity, config.head_dim)
    values = torch.zeros_like(keys)
    torch.manual_seed(42)
    sequence = torch.randn(1,8,config.hidden_size)
    errors = []
    with torch.inference_mode():
        for position in range(8):
            sample = (sequence[:,position:position+1], keys, values, *controls(talker.model,position,args.capacity))
            logits, hidden, keys, values = wrapper(*sample)
            expected = talker.codec_head(talker.model(inputs_embeds=sequence[:,:position+1],use_cache=False).last_hidden_state[:,-1:])
            torch.testing.assert_close(logits,expected,atol=2e-4,rtol=2e-4)
            errors.append((logits-expected).abs().max().item())
        print(json.dumps({'cached_steps':8,'fp32_max_abs_error':max(errors)}),flush=True)
        traced = torch.jit.trace(wrapper,sample,strict=False,check_trace=False)
    names = ['embeddings','keys','values','write_mask','attention_mask','cosine','sine']
    converted = ct.convert(traced,convert_to='mlprogram',
        inputs=[ct.TensorType(name=n,shape=t.shape,dtype=np.float32) for n,t in zip(names,sample)],
        outputs=[ct.TensorType(name=n) for n in ['logits','hidden','next_keys','next_values']],
        compute_precision=ct.precision.FLOAT16,minimum_deployment_target=ct.target.macOS15)
    args.output.parent.mkdir(parents=True,exist_ok=True)
    converted.save(args.output)
    args.output.with_suffix('.parity.json').write_text(json.dumps({'cached_steps':8,'fp32_max_abs_error':max(errors)},indent=2)+'\n')
    print(f'Saved {args.output}',flush=True)


if __name__ == '__main__':
    main()
