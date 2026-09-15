"""Batched causal prefill with KV outputs compatible with cached VoiceDesign blocks.

Pinned source: QwenLM/Qwen3-TTS 022e286b98fbec7e1e916cb940cdf532cd9f488e.
No persistent prompt/audio cache: every input is freshly processed.
"""
import argparse
import json
from pathlib import Path

import coremltools as ct
import numpy as np
import torch
from export_qwen_cached_step import controls
from export_voice_cached_blocks import CachedBlock
from qwen_tts.core.models.configuration_qwen3_tts import Qwen3TTSTalkerConfig
from qwen_tts.core.models.modeling_qwen3_tts import (
    Qwen3TTSTalkerForConditionalGeneration,
    repeat_kv,
    rotate_half,
)
from safetensors.torch import load_file


def load_talker(source):
    """Load only talker weights; prefill export does not need the audio tokenizer."""
    config = json.loads((source / "config.json").read_text())
    talker = Qwen3TTSTalkerForConditionalGeneration(
        Qwen3TTSTalkerConfig(**config["talker_config"])
    )
    state = {
        name.removeprefix("talker."): value
        for name, value in load_file(source / "model.safetensors", device="cpu").items()
        if name.startswith("talker.")
    }
    result = talker.load_state_dict(state, strict=True)
    if result.missing_keys or result.unexpected_keys:
        raise RuntimeError(f"Talker checkpoint mismatch: {result}")
    return talker.eval()


class PrefillCache(CachedBlock):
    def forward(self, embeddings, cosine, sine, attention_mask):
        hidden = embeddings
        length = embeddings.shape[1]
        keys, values = [], []
        for layer in self.model.layers:
            residual = hidden
            norm = layer.input_layernorm(hidden)
            attn = layer.self_attn
            shape = (1,length,-1,attn.head_dim)
            q = attn.q_norm(attn.q_proj(norm).view(shape)).transpose(1,2)
            k = attn.k_norm(attn.k_proj(norm).view(shape)).transpose(1,2)
            v = attn.v_proj(norm).view(shape).transpose(1,2)
            q = q*cosine + rotate_half(q)*sine
            k = k*cosine + rotate_half(k)*sine
            if not getattr(self, "first_frame_only", False):
                keys.append(k if getattr(self,'compact_kv',False) else torch.nn.functional.pad(k,(0,0,0,128-length)))
                values.append(v if getattr(self,'compact_kv',False) else torch.nn.functional.pad(v,(0,0,0,128-length)))
            scores = torch.matmul(q,repeat_kv(k,attn.num_key_value_groups).transpose(2,3))*attn.scaling
            probs = torch.softmax(scores+attention_mask,dim=-1)
            output = torch.matmul(probs,repeat_kv(v,attn.num_key_value_groups))
            hidden = residual + attn.o_proj(output.transpose(1,2).reshape(1,length,-1))
            hidden = hidden + layer.mlp(layer.post_attention_layernorm(hidden))
        if getattr(self,'last_token_only',False):
            hidden = hidden[:,-1:]
        hidden = self.model.norm(hidden)
        if getattr(self, "first_frame_only", False):
            return self.head(hidden), hidden
        return self.head(hidden),hidden,torch.cat(keys,dim=0),torch.cat(values,dim=0)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('source',type=Path)
    parser.add_argument('--block',type=int,choices=range(4),default=0)
    parser.add_argument('--all-layers',action='store_true')
    parser.add_argument('--length',type=int,default=64)
    parser.add_argument('--output',type=Path,required=True)
    parser.add_argument('--w8',action='store_true',help='INT8 matrix weights with FP16 activations')
    parser.add_argument(
        '--palettize-bits',
        type=int,
        choices=[4, 6, 8],
        help='K-means LUT weight compression; activations and unselected ops remain FP16',
    )
    parser.add_argument('--compact-kv',action='store_true',help='Return only populated prefix KV; runtime pads when continuation begins')
    parser.add_argument('--last-token-only',action='store_true',help='Only normalize/project the final token; requires all layers and exact-length runtime input')
    parser.add_argument(
        '--first-frame-only',
        action='store_true',
        help='Return logits and hidden only; continuation must rebuild exact KV separately',
    )
    args = parser.parse_args()
    if args.output.exists() or not 1 <= args.length <= 64:
        parser.error('output must be new; length must be 1..64')
    if args.last_token_only and not args.all_layers:
        parser.error('--last-token-only requires --all-layers')
    if args.first_frame_only and (not args.last_token_only or not args.all_layers):
        parser.error('--first-frame-only requires --all-layers and --last-token-only')
    if args.w8 and args.palettize_bits:
        parser.error('--w8 and --palettize-bits are mutually exclusive')
    torch.set_num_threads(4)
    talker = load_talker(args.source)
    start,end = (0,talker.config.num_hidden_layers) if args.all_layers else (args.block*7,(args.block+1)*7)
    wrapper = PrefillCache(talker,start,end).eval()
    wrapper.compact_kv = args.compact_kv
    wrapper.last_token_only = args.last_token_only
    wrapper.first_frame_only = args.first_frame_only
    reference = CachedBlock(talker,start,end).eval()
    torch.manual_seed(42)
    embeddings = torch.randn(1,args.length,talker.config.hidden_size)*0.5
    positions = torch.arange(args.length).reshape(1,1,-1).expand(3,1,-1)
    cos,sin = talker.model.rotary_emb(embeddings,positions)
    mask = torch.triu(torch.full((1,1,args.length,args.length),float('-inf')),diagonal=1)
    sample = (embeddings,cos[0].unsqueeze(1),sin[0].unsqueeze(1),mask)
    with torch.inference_mode():
        actual = wrapper(*sample)
        cfg = talker.config
        keys = torch.zeros(end-start,cfg.num_key_value_heads,128,cfg.head_dim)
        values = torch.zeros_like(keys)
        for pos in range(args.length):
            logits,_hidden,keys,values = reference(embeddings[:,pos:pos+1],keys,values,
                *controls(talker.model,pos,128))
            if not args.last_token_only or pos == args.length-1:
                observed = actual[0] if args.last_token_only else actual[0][:,pos:pos+1]
                torch.testing.assert_close(observed,logits,atol=1e-3,rtol=1e-3)
        if not args.first_frame_only:
            torch.testing.assert_close(actual[2],keys[:,:,:args.length] if args.compact_kv else keys,atol=1e-3,rtol=1e-3)
            torch.testing.assert_close(actual[3],values[:,:,:args.length] if args.compact_kv else values,atol=1e-3,rtol=1e-3)
        print(f'FP32 prefill/KV parity PASS: {args.length} positions',flush=True)
        traced = torch.jit.trace(wrapper,sample,strict=False,check_trace=False)
    converted = ct.convert(traced,convert_to='mlprogram',
        inputs=[ct.TensorType(name=n,shape=x.shape,dtype=np.float32) for n,x in zip(
            ['embeddings','cosine','sine','attention_mask'],sample, strict=True)],
        outputs=[ct.TensorType(name=n) for n in (
            ['logits','hidden'] if args.first_frame_only
            else ['logits','hidden','next_keys','next_values']
        )],
        minimum_deployment_target=ct.target.macOS15,compute_precision=ct.precision.FLOAT16,
        skip_model_load=True)
    args.output.parent.mkdir(parents=True,exist_ok=True)
    if args.w8:
        from coremltools.optimize.coreml import (
            OpLinearQuantizerConfig,
            OptimizationConfig,
            linear_quantize_weights,
        )
        op = OpLinearQuantizerConfig(mode='linear_symmetric',dtype='int8',granularity='per_channel')
        converted = linear_quantize_weights(converted,config=OptimizationConfig(
            op_type_configs={name:op for name in ['linear','conv','matmul']}))
    elif args.palettize_bits:
        from coremltools.optimize.coreml import (
            OpPalettizerConfig,
            OptimizationConfig,
            palettize_weights,
        )

        op = OpPalettizerConfig(mode='kmeans', nbits=args.palettize_bits)
        converted = palettize_weights(
            converted,
            config=OptimizationConfig(
                op_type_configs={name: op for name in ['linear', 'conv', 'matmul']}
            ),
        )
    converted.save(args.output)
    np.savez(args.output.with_suffix('.inputs.npz'),**{n:x.numpy() for n,x in zip(
        ['embeddings','cosine','sine','attention_mask'],sample, strict=True)},
        expected_hidden=actual[1].numpy(),
        **({} if args.first_frame_only else {
            'expected_keys':actual[2].numpy(),'expected_values':actual[3].numpy()
        }))
    print(f'Saved candidate: {args.output}',flush=True)


if __name__=='__main__':
    main()
