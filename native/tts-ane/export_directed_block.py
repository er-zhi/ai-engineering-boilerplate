"""Split directed VoiceDesign prefill into independently verifiable ANE blocks."""
import argparse
import json
from pathlib import Path
import coremltools as ct
import numpy as np
import torch
from qwen_tts import Qwen3TTSModel
from qwen_tts.core.models import modeling_qwen3_tts as source
from export_qwen06_talker_prefill import text_rotary


class StableRMS(torch.nn.Module):
    def __init__(self, norm):
        super().__init__()
        self.weight = norm.weight
        self.eps = norm.variance_epsilon

    def forward(self, value):
        scaled = value / 4.0
        return scaled * torch.rsqrt((scaled*scaled).mean(-1,keepdim=True)+self.eps/16.0) * self.weight


def stable_norms(module):
    for name,child in list(module.named_children()):
        if isinstance(child,source.Qwen3TTSRMSNorm):
            setattr(module,name,StableRMS(child))
        else:
            stable_norms(child)


class Block(torch.nn.Module):
    def __init__(self, talker, start, end, length):
        super().__init__()
        self.layers = torch.nn.ModuleList(talker.model.layers[start:end])
        self.final = end == len(talker.model.layers)
        self.norm = talker.model.norm if self.final else torch.nn.Identity()
        self.head = talker.codec_head if self.final else torch.nn.Identity()
        positions = torch.arange(length).view(1,1,-1).expand(3,1,-1)
        cos,sin = talker.model.rotary_emb(torch.zeros(1,length,talker.config.hidden_size),positions)
        self.register_buffer('cosine',cos.detach())
        self.register_buffer('sine',sin.detach())
        self.register_buffer('mask',torch.triu(torch.full((1,1,length,length),float('-inf')),diagonal=1))

    def forward(self, hidden):
        for layer in self.layers:
            hidden = layer(hidden,attention_mask=self.mask,position_embeddings=(self.cosine,self.sine),
                use_cache=False,output_attentions=False)[0]
        if self.final:
            hidden = self.norm(hidden[:,-1:])
            return self.head(hidden),hidden
        return hidden


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('model',type=Path)
    parser.add_argument('fixture',type=Path)
    parser.add_argument('--start',type=int,required=True)
    parser.add_argument('--end',type=int,required=True)
    parser.add_argument('--output',type=Path,required=True)
    parser.add_argument('--stable-rms',action='store_true')
    args = parser.parse_args()
    torch.set_num_threads(4)
    talker = Qwen3TTSModel.from_pretrained(str(args.model),dtype=torch.float32,device_map='cpu',
        local_files_only=True,attn_implementation='eager').model.talker.eval()
    if not 0 <= args.start < args.end <= len(talker.model.layers):
        parser.error('invalid layer range')
    source.apply_multimodal_rotary_pos_emb = text_rotary
    sample = torch.from_numpy(np.load(args.fixture)['embeddings'])
    with torch.inference_mode():
        if args.start:
            sample = Block(talker,0,args.start,sample.shape[1])(sample)
        wrapper = Block(talker,args.start,args.end,sample.shape[1]).eval()
        expected = wrapper(sample)
        if args.stable_rms:
            stable_norms(wrapper)
        actual = wrapper(sample)
        check_actual,check_expected = (actual[0],expected[0]) if wrapper.final else (actual,expected)
        torch.testing.assert_close(check_actual,check_expected,atol=1e-4,rtol=1e-4)
        traced = torch.jit.trace(wrapper,sample,strict=False,check_trace=False)
    converted = ct.convert(traced,convert_to='mlprogram',
        inputs=[ct.TensorType(name='embeddings',shape=sample.shape,dtype=np.float32)],
        outputs=[ct.TensorType(name=n) for n in (['logits','hidden'] if wrapper.final else ['hidden'])],
        compute_precision=ct.precision.FLOAT16,minimum_deployment_target=ct.target.macOS15)
    args.output.parent.mkdir(parents=True,exist_ok=True)
    converted.save(args.output)
    np.savez(args.output.with_suffix('.inputs.npz'),embeddings=sample.numpy(),expected=check_expected.numpy())
    print(json.dumps({'saved':str(args.output),'start':args.start,'end':args.end,'stable_rms':args.stable_rms}),flush=True)


if __name__=='__main__':
    main()
