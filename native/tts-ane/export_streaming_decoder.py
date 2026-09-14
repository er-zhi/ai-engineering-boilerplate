"""State-preserving Qwen decoder: one frame, true sliding KV and conv histories.

First validate against the complete original FP32 waveform, including beyond
the 72-position attention window. Core ML export is a separate candidate.
"""
import argparse
import copy
import json
from pathlib import Path

import coremltools as ct
import numpy as np
import torch
from qwen_tts.core.tokenizer_12hz.modeling_qwen3_tts_tokenizer_v2 import (
    Qwen3TTSTokenizerV2Model, Qwen3TTSTokenizerV2CausalConvNet,
    Qwen3TTSTokenizerV2CausalTransConvNet, rotate_half, repeat_kv,
)
from export_qwen12hz_decoder import PhaseUpsampleConv


def pairwise_sum(parts):
    while len(parts)>1:
        parts = [parts[i]+parts[i+1] for i in range(0,len(parts),2)]
    return parts[0]


class SplitLinear(torch.nn.Module):
    def __init__(self, source, parts):
        super().__init__()
        if source.in_features % parts:
            raise ValueError('Linear input dimension must divide evenly')
        self.weight,self.bias = source.weight,source.bias
        self.parts,self.width = parts,source.in_features//parts

    def forward(self,x):
        result = pairwise_sum([torch.nn.functional.linear(x[...,i*self.width:(i+1)*self.width],
            self.weight[:,i*self.width:(i+1)*self.width]) for i in range(self.parts)])
        return result if self.bias is None else result+self.bias


class SplitConv(torch.nn.Module):
    def __init__(self, source, parts):
        super().__init__()
        if source.groups != 1 or source.in_channels % parts:
            raise ValueError('Split convolution requires divisible dense channels')
        self.weight,self.bias = source.weight,source.bias
        self.stride,self.padding,self.dilation = source.stride,source.padding,source.dilation
        self.parts,self.width = parts,source.in_channels//parts

    def forward(self,x):
        result = pairwise_sum([torch.nn.functional.conv1d(x[:,i*self.width:(i+1)*self.width],
            self.weight[:,i*self.width:(i+1)*self.width],stride=self.stride,
            padding=self.padding,dilation=self.dilation) for i in range(self.parts)])
        return result if self.bias is None else result+self.bias.reshape(1,-1,1)


def split_matrix_reductions(module,parts):
    for name,child in list(module.named_children()):
        if isinstance(child,torch.nn.Linear):
            setattr(module,name,SplitLinear(child,parts))
        elif isinstance(child,torch.nn.Conv1d) and child.groups == 1:
            setattr(module,name,SplitConv(child,parts))
        else:
            split_matrix_reductions(child,parts)


class MatmulLinear(torch.nn.Module):
    def __init__(self, source):
        super().__init__()
        self.weight = source.weight
        self.bias = source.bias

    def forward(self, x):
        result = torch.matmul(x, self.weight.transpose(0,1))
        return result if self.bias is None else result+self.bias


class MatmulConv(torch.nn.Module):
    """Exact stride-one, unpadded Conv1d as patch matrix multiplication."""
    def __init__(self, source):
        super().__init__()
        if source.stride != (1,) or source.padding != (0,):
            raise ValueError('Matmul convolution expects explicit padding and stride one')
        if source.groups not in (1, source.in_channels) or (source.groups != 1 and source.out_channels != source.in_channels):
            raise ValueError('Only dense or depthwise convolution is supported')
        self.weight, self.bias = source.weight, source.bias
        self.kernel = source.kernel_size[0]
        self.dilation = source.dilation[0]
        self.depthwise = source.groups != 1

    def forward(self, x):
        steps = x.shape[-1]-(self.kernel-1)*self.dilation
        patches = torch.stack([x[...,i*self.dilation:i*self.dilation+steps]
            for i in range(self.kernel)],dim=-1)
        if self.depthwise:
            result = torch.matmul(patches,self.weight.transpose(1,2)).squeeze(-1)
        else:
            patches = patches.permute(0,2,1,3).reshape(x.shape[0],steps,-1)
            result = torch.matmul(patches,self.weight.reshape(self.weight.shape[0],-1).transpose(0,1)).transpose(1,2)
        return result if self.bias is None else result+self.bias.reshape(1,-1,1)


def replace_matrix_ops(module):
    for name, child in list(module.named_children()):
        if isinstance(child,torch.nn.Linear):
            setattr(module,name,MatmulLinear(child))
        elif isinstance(child,torch.nn.Conv1d):
            setattr(module,name,MatmulConv(child))
        else:
            replace_matrix_ops(child)


class ScaledDecoderRMS(torch.nn.Module):
    """Keep small variance and epsilon representable in FP16; same real equation."""
    def __init__(self, source, scale):
        super().__init__()
        self.weight = source.weight
        self.epsilon = source.variance_epsilon * scale * scale
        self.scale = scale

    def forward(self, hidden):
        scaled = hidden * self.scale
        return self.weight * scaled * torch.rsqrt(scaled.square().mean(-1,keepdim=True)+self.epsilon)


def scale_decoder_norms(module, scale):
    from qwen_tts.core.tokenizer_12hz.modeling_qwen3_tts_tokenizer_v2 import Qwen3TTSTokenizerV2DecoderRMSNorm
    for name, child in list(module.named_children()):
        if isinstance(child, Qwen3TTSTokenizerV2DecoderRMSNorm):
            setattr(module, name, ScaledDecoderRMS(child, scale))
        else:
            scale_decoder_norms(child, scale)


class StreamingConv(torch.nn.Module):
    def __init__(self,source):
        super().__init__()
        if source.conv.stride != (1,):
            raise ValueError('Decoder streaming supports stride-one causal convolutions')
        self.conv = source.conv
        self.padding = source.padding
        if self.padding:
            self.register_buffer('history',torch.zeros(1,1,self.padding,self.conv.in_channels))

    def forward(self,x):
        if self.padding:
            joined = torch.cat((self.history.squeeze(1).transpose(1,2),x),dim=-1)
            result = self.conv(joined)
            self.history[:] = joined[...,-self.padding:].transpose(1,2).unsqueeze(1)
            return result
        return self.conv(x)


class StreamingUpsample(PhaseUpsampleConv):
    def __init__(self,source):
        super().__init__(source)
        if self.has_previous:
            self.register_buffer('history',torch.zeros(1,1,1,self.in_channels))

    def forward(self,x):
        y = self.current(x)
        if self.has_previous:
            previous = torch.cat((self.history.squeeze(1).transpose(1,2),x[...,:-1]),dim=-1)
            y = y+self.previous(previous)
            self.history[:] = x[...,-1:].transpose(1,2).unsqueeze(1)
        batch,_,steps = y.shape
        return y.reshape(batch,self.out_channels,self.factor,steps).permute(0,1,3,2).reshape(batch,self.out_channels,steps*self.factor)


def replace_convolutions(module):
    for name,child in list(module.named_children()):
        if isinstance(child,Qwen3TTSTokenizerV2CausalConvNet):
            setattr(module,name,StreamingConv(child))
        elif isinstance(child,Qwen3TTSTokenizerV2CausalTransConvNet):
            setattr(module,name,StreamingUpsample(child.conv))
        else:
            replace_convolutions(child)


class StreamingDecoder(torch.nn.Module):
    def __init__(self,decoder):
        super().__init__()
        self.decoder = decoder
        self.debug = False
        self.debug_names = ['pre_conv', 'transformer', 'upsample_0', 'upsample_1'] + [f'wave_{i}' for i in range(len(decoder.decoder))]
        replace_convolutions(self.decoder)
        cfg = decoder.config
        self.capacity = cfg.sliding_window
        shape = (cfg.num_hidden_layers,cfg.num_key_value_heads,self.capacity,cfg.head_dim)
        self.register_buffer('key_cache',torch.zeros(shape))
        self.register_buffer('value_cache',torch.zeros(shape))

    def states(self):
        return [(name,value) for name,value in self.named_buffers()
            if name in ['key_cache','value_cache'] or name.endswith('.history')]

    def reset(self):
        for _,value in self.states():
            value.zero_()

    def forward(self,latent,cosine,sine,attention_mask):
        tf = self.decoder.pre_transformer
        convolution = self.decoder.pre_conv(latent)
        intermediates = [convolution]
        hidden = tf.input_proj(convolution.transpose(1,2))
        keys,values = [],[]
        for index,layer in enumerate(tf.layers):
            norm = layer.input_layernorm(hidden)
            attn = layer.self_attn
            shape = (1,1,-1,attn.head_dim)
            q = attn.q_norm(attn.q_proj(norm).view(shape)).transpose(1,2)
            k = attn.k_norm(attn.k_proj(norm).view(shape)).transpose(1,2)
            v = attn.v_proj(norm).view(shape).transpose(1,2)
            q,k = q*cosine+rotate_half(q)*sine,k*cosine+rotate_half(k)*sine
            k = torch.cat((self.key_cache[index:index+1,:,1:,:],k),dim=2)
            v = torch.cat((self.value_cache[index:index+1,:,1:,:],v),dim=2)
            keys.append(k)
            values.append(v)
            scores = (q @ repeat_kv(k,attn.num_key_value_groups).transpose(2,3))*attn.scaling
            attended = torch.softmax(scores+attention_mask,dim=-1) @ repeat_kv(v,attn.num_key_value_groups)
            projected = attn.o_proj(attended.transpose(1,2).reshape(1,1,-1))
            hidden = hidden+layer.self_attn_layer_scale(projected)
            hidden = hidden+layer.mlp_layer_scale(layer.mlp(layer.post_attention_layernorm(hidden)))
        self.key_cache[:] = torch.cat(keys,dim=0)
        self.value_cache[:] = torch.cat(values,dim=0)
        hidden = tf.output_proj(tf.norm(hidden)).transpose(1,2)
        intermediates.append(hidden)
        for group in self.decoder.upsample:
            for block in group:
                hidden = block(hidden)
            intermediates.append(hidden)
        for block in self.decoder.decoder:
            hidden = block(hidden)
            intermediates.append(hidden)
        pcm = hidden.clamp(-1,1).squeeze(1)
        return (pcm, *intermediates) if self.debug else pcm


def controls(decoder,position):
    cfg = decoder.config
    cos,sin = decoder.pre_transformer.rotary_emb(torch.zeros(1,1,cfg.hidden_size),torch.tensor([[position]]))
    mask = torch.full((1,1,1,cfg.sliding_window),float('-inf'))
    mask[...,-min(position+1,cfg.sliding_window):] = 0
    return cos.unsqueeze(1),sin.unsqueeze(1),mask


class ExplicitStateDecoder(torch.nn.Module):
    """Diagnostic equivalent: expose histories to isolate MLState compilation."""
    def __init__(self, decoder):
        super().__init__()
        self.step = decoder
        self.names = [name for name, _ in decoder.states()]

    def forward(self, latent, cosine, sine, attention_mask, *states):
        for name, value in zip(self.names, states):
            parent, _, leaf = name.rpartition('.')
            module = self.step.get_submodule(parent) if parent else self.step
            setattr(module, leaf, value.clone())
        pcm = self.step(latent, cosine, sine, attention_mask)
        outputs = pcm if isinstance(pcm, tuple) else (pcm,)
        return (*outputs, *(value for _, value in self.step.states()))


class PackedStateDecoder(torch.nn.Module):
    """Diagnostic: one aligned state allocation, identical logical histories."""
    def __init__(self, decoder):
        super().__init__()
        self.step = ExplicitStateDecoder(decoder)
        self.shapes = [tuple(value.shape) for _, value in decoder.states()]
        self.sizes = [int(np.prod(shape)) for shape in self.shapes]
        total = sum(self.sizes)
        if total % 32:
            raise ValueError('Packed state must be 32-element aligned')
        self.register_buffer('packed', torch.zeros(1,1,total//32,32))

    def forward(self, latent, cosine, sine, attention_mask):
        flat = self.packed.reshape(-1)
        offset, states = 0, []
        for shape, size in zip(self.shapes, self.sizes):
            states.append(flat[offset:offset+size].reshape(shape))
            offset += size
        result = self.step(latent, cosine, sine, attention_mask, *states)
        self.packed[:] = torch.cat([value.reshape(-1) for value in result[1:]]).reshape(self.packed.shape)
        return result[0]


def remove_full_slice_updates(program):
    """An exact full-tensor assignment is its update, not a scatter operation."""
    count = 0
    for function in program.functions.values():
        for op in list(function.operations):
            if op.op_type != 'slice_update' or op.x.shape != op.update.shape:
                continue
            rank = len(op.x.shape)
            def values(name, default):
                value = getattr(op, name, None)
                return [default]*rank if value is None else value.val.tolist()
            begin, end = values('begin', 0), values('end', 0)
            stride = values('stride', 1)
            bm, em = values('begin_mask', False), values('end_mask', False)
            if any(values('squeeze_mask', False)):
                continue
            if not all(slice(None if bm[i] else begin[i], None if em[i] else end[i], stride[i]).indices(size)
                == (0,size,1) for i,size in enumerate(op.x.shape)):
                continue
            with function:
                replacement = op.update
                if op.outputs[0] in function.outputs and replacement in function.inputs.values():
                    from coremltools.converters.mil.mil import Builder as mb
                    replacement = mb.identity(x=replacement, before_op=op)
                function.replace_uses_of_var_after_op(op, op.outputs[0], replacement)
                function.remove_ops([op])
            count += 1
    program.validate()
    print(f'Removed {count} exact full-buffer slice updates',flush=True)
    return program


def force_matmul_linear(program):
    """Torch frontend canonicalizes constant matmul to linear; undo for probe."""
    from coremltools.converters.mil.mil import Builder as mb
    count = 0
    for function in program.functions.values():
        for op in list(function.operations):
            if op.op_type != 'linear':
                continue
            with function:
                replacement = mb.matmul(x=op.x,y=op.weight,transpose_y=True,before_op=op)
                if op.bias is not None:
                    replacement = mb.add(x=replacement,y=op.bias,before_op=op)
                function.replace_uses_of_var_after_op(op,op.outputs[0],replacement)
                function.remove_ops([op])
            count += 1
    print(f'Replaced {count} lowered linear operations with matmul',flush=True)
    return program


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('source',type=Path)
    parser.add_argument('codes_report',type=Path)
    parser.add_argument('--report',type=Path,required=True)
    parser.add_argument('--output',type=Path,help='Export only after FP32 recurrence parity passes')
    parser.add_argument('--explicit-state',action='store_true',help='Diagnostic tensor-state export to isolate ANE compiler failure')
    parser.add_argument('--packed-state',action='store_true',help='Diagnostic single aligned state allocation')
    parser.add_argument('--debug-stages',action='store_true',help='Expose decoder stages to localize numerical divergence')
    parser.add_argument('--rms-scale',type=float,default=1.0,choices=[1.0,8.0,16.0,32.0],help='Equivalent normalization rescaling; validate against unchanged source')
    parser.add_argument('--fp32-diagnostic',action='store_true',help='Numerical conversion isolation only, not an ANE runtime candidate')
    parser.add_argument('--matmul-form',action='store_true',help='Equivalent convolution and linear matmul formulation')
    parser.add_argument('--split-matrices',type=int,choices=[1,4,8,16],default=1,help='Pairwise reduction of dense channel partitions')
    args = parser.parse_args()
    if args.explicit_state and args.packed_state:
        parser.error('Choose either explicit or packed state')
    if args.debug_stages and not args.explicit_state:
        parser.error('Debug stages currently require explicit state')
    if args.fp32_diagnostic and not args.explicit_state:
        parser.error('FP32 diagnostic requires explicit states')
    if args.matmul_form and args.split_matrices != 1:
        parser.error('Keep matrix formulation experiments separate')
    if args.report.exists() or (args.output and args.output.exists()):
        parser.error('Use new report and model paths')
    torch.set_num_threads(4)
    tokenizer = Qwen3TTSTokenizerV2Model.from_pretrained(args.source/'speech_tokenizer',
        dtype=torch.float32,local_files_only=True).eval()
    original = tokenizer.decoder
    wrapper = StreamingDecoder(copy.deepcopy(original)).eval()
    if args.rms_scale != 1.0:
        scale_decoder_norms(wrapper, args.rms_scale)
    if args.matmul_form:
        replace_matrix_ops(wrapper)
    if args.split_matrices != 1:
        split_matrix_reductions(wrapper,args.split_matrices)
    record = json.loads(args.codes_report.read_text())
    sequence = torch.tensor([chunk['codes'] for chunk in record['chunks']],dtype=torch.long).T.unsqueeze(0)
    # Extend the numerical fixture beyond the real KV window, not for listening.
    extended = sequence.repeat(1,1,(96+sequence.shape[-1]-1)//sequence.shape[-1])[...,:96]
    errors = []
    with torch.inference_mode():
        for label,codes in [('real_phrase',sequence),('attention_rollover',extended)]:
            wrapper.reset()
            numerical = PackedStateDecoder(wrapper).eval() if args.packed_state else wrapper
            expected = original(codes)[0,0]
            latent = original.quantizer.decode(codes)
            chunks = []
            for position in range(codes.shape[-1]):
                chunks.append(numerical(latent[...,position:position+1],*controls(original,position))[0].clone())
            actual = torch.cat(chunks)
            error = (actual-expected).abs()
            result = {'case':label,'frames':codes.shape[-1],'max_abs_error':error.max().item(),
                'rms_error':error.square().mean().sqrt().item()}
            print(json.dumps(result),flush=True)
            errors.append(result)
            torch.testing.assert_close(actual,expected,atol=1e-4,rtol=1e-4)
        wrapper.reset()
        wrapper.debug = args.debug_stages
        sample = (latent[...,:1],*controls(original,0))
        state_shapes = [(name, value.shape) for name, value in wrapper.states()]
        if args.explicit_state:
            sample = (*sample, *(value.clone() for _, value in wrapper.states()))
        export_wrapper = ExplicitStateDecoder(wrapper).eval() if args.explicit_state else wrapper
        if args.packed_state:
            export_wrapper = PackedStateDecoder(wrapper).eval()
            state_shapes = [('packed', export_wrapper.packed.shape)]
        traced = torch.jit.trace(export_wrapper,
            sample,strict=False,check_trace=False) if args.output else None
    args.report.parent.mkdir(parents=True,exist_ok=True)
    args.report.write_text(json.dumps({'status':'FP32_PARITY_PASS','cases':errors,
        'state_buffers':[(name,list(value.shape)) for name,value in wrapper.states()],
        'coreml_validated':False},indent=2)+'\n')
    if args.output:
        state_names = [name.replace('.', '_') for name, _ in state_shapes]
        input_names = ['latent','cosine','sine','attention_mask']
        if args.explicit_state:
            input_names += ['in_'+name for name in state_names]
        pipeline = ct.PassPipeline.DEFAULT
        if args.matmul_form:
            pipeline.remove_passes({'common::fuse_matmul_weight_bias'})
        program = ct.convert(traced,convert_to='milinternal',
            inputs=[ct.TensorType(name=name,shape=value.shape,dtype=np.float32) for name,value in zip(
                input_names,sample)],
            outputs=[ct.TensorType(name=name) for name in ['pcm']+(['debug_'+name for name in wrapper.debug_names] if args.debug_stages else [])+(['out_'+name for name in state_names] if args.explicit_state else [])],
            states=[] if args.explicit_state else [ct.StateType(name=name,wrapped_type=ct.TensorType(shape=shape,dtype=np.float16))
                for name,shape in state_shapes],
            compute_precision=ct.precision.FLOAT32 if args.fp32_diagnostic else ct.precision.FLOAT16,minimum_deployment_target=ct.target.macOS15,
            skip_model_load=True,pass_pipeline=pipeline)
        program = remove_full_slice_updates(program)
        if args.matmul_form:
            program = force_matmul_linear(program)
        converted = ct.convert(program,convert_to='mlprogram',
            minimum_deployment_target=ct.target.macOS15,skip_model_load=True,
            compute_precision=ct.precision.FLOAT32 if args.fp32_diagnostic else ct.precision.FLOAT16,
            pass_pipeline=ct.PassPipeline.EMPTY)
        args.output.parent.mkdir(parents=True,exist_ok=True)
        converted.save(args.output)
        print(f'Saved candidate {args.output}',flush=True)


if __name__ == '__main__':
    main()
