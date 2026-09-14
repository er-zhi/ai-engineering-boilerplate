"""Export four cached VoiceDesign blocks for the listening prototype."""
import argparse
from pathlib import Path
import coremltools as ct
import numpy as np
import torch
from qwen_tts import Qwen3TTSModel
from export_qwen_cached_step import CachedStep, controls
from export_directed_block import stable_norms


class CachedBlock(CachedStep):
    def __init__(self,talker,start,end):
        torch.nn.Module.__init__(self)
        self.model = torch.nn.Module()
        self.model.layers = torch.nn.ModuleList(talker.model.layers[start:end])
        self.model.norm = talker.model.norm if end==28 else torch.nn.Identity()
        self.head = talker.codec_head if end==28 else torch.nn.Identity()
        stable_norms(self)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('source',type=Path)
    parser.add_argument('--output',type=Path,required=True)
    parser.add_argument('--block-count',type=int,choices=[1,4],default=4)
    parser.add_argument('--name-prefix',default='qwen17')
    args = parser.parse_args()
    torch.set_num_threads(4)
    talker = Qwen3TTSModel.from_pretrained(str(args.source),dtype=torch.float32,device_map='cpu',
        local_files_only=True,attn_implementation='eager').model.talker.eval()
    cfg = talker.config
    layers_per_block = cfg.num_hidden_layers//args.block_count
    wrappers = [CachedBlock(talker,i*layers_per_block,(i+1)*layers_per_block).eval() for i in range(args.block_count)]
    torch.manual_seed(42)
    sequence = torch.randn(1,3,cfg.hidden_size)
    caches = [(torch.zeros(layers_per_block,cfg.num_key_value_heads,128,cfg.head_dim),
        torch.zeros(layers_per_block,cfg.num_key_value_heads,128,cfg.head_dim)) for _ in range(args.block_count)]
    samples = []
    with torch.inference_mode():
        for pos in range(3):
            hidden = sequence[:,pos:pos+1]
            samples = []
            for index,wrapper in enumerate(wrappers):
                sample = (hidden,*caches[index],*controls(talker.model,pos,128))
                samples.append(sample)
                logits,hidden,k,v = wrapper(*sample)
                caches[index] = (k,v)
            expected = talker.codec_head(talker.model(inputs_embeds=sequence[:,:pos+1],use_cache=False).last_hidden_state[:,-1:])
            torch.testing.assert_close(logits,expected,atol=1e-3,rtol=1e-3)
        print('Four-block cached FP32 chain matches source',flush=True)
        args.output.mkdir(parents=True,exist_ok=True)
        for index,(wrapper,sample) in enumerate(zip(wrappers,samples)):
            traced = torch.jit.trace(wrapper,sample,strict=False,check_trace=False)
            converted = ct.convert(traced,convert_to='mlprogram',
                inputs=[ct.TensorType(name=n,shape=x.shape,dtype=np.float32) for n,x in zip(
                    ['embeddings','keys','values','write_mask','attention_mask','cosine','sine'],sample)],
                outputs=[ct.TensorType(name=n) for n in ['logits','hidden','next_keys','next_values']],
                compute_precision=ct.precision.FLOAT16,minimum_deployment_target=ct.target.macOS15)
            output = args.output/f'{args.name_prefix}_cached_block{index}.mlpackage'
            converted.save(output)
            print(f'Saved {output}',flush=True)


if __name__=='__main__':
    main()
