"""Probe greedy residual generation inside one Core ML call; exact source check."""
import argparse
import copy
import json
from pathlib import Path
import coremltools as ct
import numpy as np
import torch
from qwen_tts import Qwen3TTSModel
from export_qwen_predictor import Predictor
from qwen_tts.core.models.modeling_qwen3_tts import rotate_half, repeat_kv


class GrowingPredictor(Predictor):
    """Unrolled residual steps need only actual past tokens, never future slots."""
    def forward(self,embeddings,keys,values,cosine,sine):
        hidden = self.projection(embeddings)
        length = embeddings.shape[1]
        if length > 1 and keys is not None:
            raise ValueError('Multi-token prefill must start with an empty cache')
        next_keys,next_values = [],[]
        for index,layer in enumerate(self.model.layers):
            residual = hidden
            norm = layer.input_layernorm(hidden)
            attn = layer.self_attn
            shape = (1,length,-1,attn.head_dim)
            q = attn.q_norm(attn.q_proj(norm).view(shape)).transpose(1,2)
            k = attn.k_norm(attn.k_proj(norm).view(shape)).transpose(1,2)
            v = attn.v_proj(norm).view(shape).transpose(1,2)
            q,k = q*cosine+rotate_half(q)*sine,k*cosine+rotate_half(k)*sine
            if keys is not None:
                k = torch.cat((keys[index:index+1],k),dim=2)
                v = torch.cat((values[index:index+1],v),dim=2)
            next_keys.append(k)
            next_values.append(v)
            scores = (q @ repeat_kv(k,attn.num_key_value_groups).transpose(2,3))*attn.scaling
            if length > 1:
                scores = scores + torch.triu(torch.full((length,length),float('-inf'),device=scores.device,dtype=scores.dtype),diagonal=1)
            output = torch.softmax(scores,dim=-1) @ repeat_kv(v,attn.num_key_value_groups)
            hidden = residual+attn.o_proj(output.transpose(1,2).reshape(1,length,-1))
            hidden = hidden+layer.mlp(layer.post_attention_layernorm(hidden))
        hidden = self.model.norm(hidden)
        return hidden,hidden,torch.cat(next_keys,dim=0),torch.cat(next_values,dim=0)


class ChannelFirstPredictor(GrowingPredictor):
    """Apple's BC1S/1x1-convolution layout, retaining source weights and attention.

    https://machinelearning.apple.com/research/neural-engine-transformers
    """
    @staticmethod
    def linear(layer,value):
        return torch.nn.functional.conv2d(value,layer.weight[:,:,None,None],layer.bias)

    @staticmethod
    def norm(layer,value):
        normalized = value*torch.rsqrt(value.square().mean(1,keepdim=True)+layer.variance_epsilon)
        return normalized*layer.weight[None,:,None,None]

    def forward(self,embeddings,keys,values,cosine,sine):
        hidden = self.projection(embeddings).transpose(1,2).unsqueeze(2)
        length = embeddings.shape[1]
        if length > 1 and keys is not None:
            raise ValueError('Multi-token prefill must start with an empty cache')
        next_keys,next_values = [],[]
        for index,layer in enumerate(self.model.layers):
            residual = hidden
            norm = self.norm(layer.input_layernorm,hidden)
            attn = layer.self_attn
            def heads(projection):
                return self.linear(projection,norm).reshape(1,-1,attn.head_dim,length).transpose(2,3)
            q,k,v = attn.q_norm(heads(attn.q_proj)),attn.k_norm(heads(attn.k_proj)),heads(attn.v_proj)
            q,k = q*cosine+rotate_half(q)*sine,k*cosine+rotate_half(k)*sine
            if keys is not None:
                k = torch.cat((keys[index:index+1],k),dim=2)
                v = torch.cat((values[index:index+1],v),dim=2)
            next_keys.append(k)
            next_values.append(v)
            scores = (q @ repeat_kv(k,attn.num_key_value_groups).transpose(2,3))*attn.scaling
            if length > 1:
                scores = scores+torch.triu(torch.full((length,length),float('-inf'),device=scores.device,dtype=scores.dtype),diagonal=1)
            output = torch.softmax(scores,dim=-1) @ repeat_kv(v,attn.num_key_value_groups)
            output = output.transpose(2,3).reshape(1,-1,1,length)
            hidden = residual+self.linear(attn.o_proj,output)
            norm = self.norm(layer.post_attention_layernorm,hidden)
            gated = layer.mlp.act_fn(self.linear(layer.mlp.gate_proj,norm))*self.linear(layer.mlp.up_proj,norm)
            hidden = hidden+self.linear(layer.mlp.down_proj,gated)
        hidden = self.norm(self.model.norm,hidden).squeeze(2).transpose(1,2)
        return hidden,hidden,torch.cat(next_keys,dim=0),torch.cat(next_values,dim=0)


class Fused(torch.nn.Module):
    def __init__(self,predictor,count,float_selection=False,growing_cache=False,batch_prefix=False,channel_first=False,re_prefill=False):
        super().__init__()
        if batch_prefix and not growing_cache:
            raise ValueError('batch_prefix requires growing_cache')
        self.batch_prefix = batch_prefix
        if re_prefill and (not growing_cache or not batch_prefix):
            raise ValueError('re_prefill requires growing_cache and batch_prefix')
        self.re_prefill = re_prefill
        self.core = GrowingPredictor(predictor) if growing_cache else Predictor(predictor)
        if channel_first:
            if not growing_cache:
                raise ValueError('channel_first requires growing_cache')
            self.core = ChannelFirstPredictor(predictor)
        self.growing_cache = growing_cache
        self.core.head = torch.nn.Identity()
        self.heads = predictor.lm_head
        self.embeddings = predictor.get_input_embeddings()
        self.count = count
        self.float_selection = float_selection
        self.register_buffer('ranks',torch.arange(predictor.lm_head[0].out_features,dtype=torch.float32).reshape(1,1,-1))
        cfg = predictor.config
        self.register_buffer('empty',torch.zeros(cfg.num_hidden_layers,cfg.num_key_value_heads,16,cfg.head_dim))
        writes,masks,cosines,sines = [],[],[],[]
        for pos in range(count+1):
            write = torch.zeros(1,1,16,1)
            write[:,:,pos,:] = 1
            mask = torch.full((1,1,1,16),float('-inf'))
            mask[...,:pos+1] = 0
            cos,sin = predictor.model.rotary_emb(torch.zeros(1,1,cfg.hidden_size),torch.tensor([[pos]]))
            writes.append(write)
            masks.append(mask)
            cosines.append(cos.unsqueeze(1))
            sines.append(sin.unsqueeze(1))
        for name,values in [('writes',writes),('masks',masks),('cosines',cosines),('sines',sines)]:
            self.register_buffer(name,torch.stack(values).detach())

    def forward(self,past_hidden,first_embedding):
        keys,values = (None,None) if self.growing_cache else (self.empty,self.empty)
        current = past_hidden
        codes = []
        prefix = torch.cat((past_hidden,first_embedding),dim=1) if self.re_prefill else None
        for pos in range(1 if self.batch_prefix else 0,self.count+1):
            if self.re_prefill:
                if pos > 1:
                    prefix = torch.cat((prefix,current),dim=1)
                _,hidden,_,_ = self.core(prefix,None,None,
                    torch.cat(tuple(self.cosines[i] for i in range(pos+1)),dim=2),
                    torch.cat(tuple(self.sines[i] for i in range(pos+1)),dim=2))
                hidden = hidden[:,-1:]
            elif self.batch_prefix and pos == 1:
                current = torch.cat((past_hidden,first_embedding),dim=1)
                _,hidden,keys,values = self.core(current,keys,values,
                    torch.cat((self.cosines[0],self.cosines[1]),dim=2),
                    torch.cat((self.sines[0],self.sines[1]),dim=2))
                hidden = hidden[:,-1:]
            elif self.growing_cache:
                _,hidden,keys,values = self.core(current,keys,values,self.cosines[pos],self.sines[pos])
            else:
                _,hidden,keys,values = self.core(current,keys,values,self.writes[pos],
                    self.masks[pos],self.cosines[pos],self.sines[pos])
            if pos == 0:
                current = first_embedding
            else:
                logits = self.heads[pos-1](hidden)
                if self.float_selection:
                    # Exact first-maximum tie breaking, without integer argmax/gather.
                    winners = (logits == logits.amax(-1,keepdim=True)).to(logits.dtype)
                    size = self.ranks.shape[-1]
                    code = size - ((size-self.ranks)*winners).amax(-1)
                else:
                    code = logits.argmax(-1)
                codes.append(code)
                if pos < self.count:
                    if self.float_selection:
                        selector = (self.ranks == code.unsqueeze(-1)).to(hidden.dtype)
                        current = selector @ self.embeddings[pos-1].weight
                    else:
                        current = self.embeddings[pos-1](code)
        return torch.cat(codes,dim=-1)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('model',type=Path)
    parser.add_argument('--output',type=Path,required=True)
    parser.add_argument('--codes',type=int,default=2)
    parser.add_argument('--float-selection',action='store_true',help='Exact floating-point selection avoids integer argmax and gather')
    parser.add_argument('--growing-cache',action='store_true',help='Specialize each unrolled step to its true causal prefix length')
    parser.add_argument('--batch-prefix',action='store_true',help='Process the two known initial predictor tokens together; requires growing-cache')
    parser.add_argument('--re-prefill',action='store_true',help='Experimental full short-prefix recomputation; requires growing-cache and batch-prefix')
    parser.add_argument('--channel-first',action='store_true',help='Experimental BC1S convolution layout; requires growing-cache')
    parser.add_argument('--stable-rms',action='store_true',help='Algebraically rescale RMS normalization before FP16 export')
    parser.add_argument('--validation-samples',type=Path,help='Captured real predictor inputs for exact FP32 source-code parity before export')
    parser.add_argument('--fp32-diagnostic',action='store_true',help='Export FP32 graph for CPU calibration validation; not an ANE production candidate')
    parser.add_argument('--outlier-scale',type=int,choices=[256,1024,4096],help='Experimental Qwen06 layer-2 channel-2016 equivalent weight scaling; requires captured source validation')
    args = parser.parse_args()
    if args.output.exists():
        parser.error('output already exists; preserve previous candidates')
    if args.outlier_scale and (not args.validation_samples or args.stable_rms or args.channel_first):
        parser.error('Outlier scaling requires source validation and unmodified standard-layout norms')
    if args.batch_prefix and not args.growing_cache:
        parser.error('--batch-prefix requires --growing-cache')
    if args.re_prefill and (not args.batch_prefix or not args.growing_cache):
        parser.error('--re-prefill requires --batch-prefix and --growing-cache')
    if args.validation_samples and args.stable_rms:
        parser.error('Source validation requires unmodified source norms; do not combine with --stable-rms')
    if args.channel_first and (not args.growing_cache or args.stable_rms):
        parser.error('--channel-first requires --growing-cache and original RMS norms')
    if not 1 <= args.codes <= 15:
        parser.error('codes must be 1..15')
    torch.set_num_threads(4)
    tts = Qwen3TTSModel.from_pretrained(str(args.model),dtype=torch.float32,device_map='cpu',
        local_files_only=True,attn_implementation='eager')
    predictor = tts.model.talker.code_predictor.eval()
    source_predictor = predictor
    if args.outlier_scale:
        source_predictor = copy.deepcopy(predictor)
        mlp = predictor.model.layers[2].mlp
        if mlp.up_proj.bias is not None or mlp.up_proj.out_features <= 2016:
            raise ValueError('Unsupported outlier-scaling architecture')
        with torch.no_grad():
            mlp.up_proj.weight[2016].div_(args.outlier_scale)
            mlp.down_proj.weight[:,2016].mul_(args.outlier_scale)
    torch.manual_seed(42)
    hidden = torch.randn(1,1,tts.model.talker.config.hidden_size)
    first = tts.model.talker.get_input_embeddings()(torch.tensor([[765]])).detach()
    wrapper = Fused(predictor,args.codes,args.float_selection,args.growing_cache,args.batch_prefix,args.channel_first,args.re_prefill).eval()
    with torch.inference_mode():
        expected = source_predictor.generate(inputs_embeds=torch.cat((hidden,first),dim=1),
            max_new_tokens=args.codes,do_sample=False)
        if args.stable_rms:
            from export_directed_block import stable_norms
            stable_norms(wrapper.core)
        actual = wrapper(hidden,first)
        torch.testing.assert_close(actual.to(expected.dtype),expected,atol=0,rtol=0)
        print(json.dumps({'source_codes':expected.tolist(),'fused_codes':actual.tolist()}),flush=True)
        if args.validation_samples:
            with np.load(args.validation_samples,allow_pickle=False) as data:
                samples = [(torch.from_numpy(a.copy()),torch.from_numpy(b.copy()))
                    for a,b in zip(data['past_hidden'],data['first_embedding'],strict=True)]
            if not samples:
                raise ValueError('Validation samples must not be empty')
            for sample_hidden,sample_first in samples:
                source = source_predictor.generate(inputs_embeds=torch.cat((sample_hidden,sample_first),dim=1),
                    max_new_tokens=args.codes,do_sample=False)
                candidate = wrapper(sample_hidden,sample_first)
                torch.testing.assert_close(candidate.to(source.dtype),source,atol=0,rtol=0)
            print(json.dumps({'validated_frames':len(samples),'different_source_codes':0}),flush=True)
        traced = torch.jit.trace(wrapper,(hidden,first),strict=False,check_trace=False)
    converted = ct.convert(traced,convert_to='mlprogram',
        inputs=[ct.TensorType(name=n,shape=x.shape,dtype=np.float32) for n,x in [('past_hidden',hidden),('first_embedding',first)]],
        outputs=[ct.TensorType(name='codes')],compute_precision=(ct.precision.FLOAT32 if args.fp32_diagnostic else ct.precision.FLOAT16),
        minimum_deployment_target=ct.target.macOS15)
    args.output.parent.mkdir(parents=True,exist_ok=True)
    converted.save(args.output)
    np.savez(args.output.with_suffix('.inputs.npz'),past_hidden=hidden.numpy(),first_embedding=first.numpy(),expected=expected.numpy())
    print(f'Saved {args.output}',flush=True)


if __name__=='__main__':
    main()
