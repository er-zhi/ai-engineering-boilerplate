"""Static Qwen talker prefill export; not an autoregressive TTS runtime."""
import argparse
import json
from pathlib import Path

import coremltools as ct
import numpy as np
import torch
from qwen_tts import Qwen3TTSModel
from qwen_tts.core.models import modeling_qwen3_tts as source


def text_rotary(q, k, cos, sin, mrope_section, mrope_interleaved=False, unsqueeze_dim=1):
    # This exporter supplies identical temporal/height/width positions for text.
    # Selecting the first plane is equivalent without strided in-place updates.
    cos, sin = cos[0].unsqueeze(unsqueeze_dim), sin[0].unsqueeze(unsqueeze_dim)
    return q * cos + source.rotate_half(q) * sin, k * cos + source.rotate_half(k) * sin


class Prefill(torch.nn.Module):
    def __init__(self, talker, length, with_hidden=False):
        super().__init__()
        self.model = talker.model
        self.head = talker.codec_head
        self.with_hidden = with_hidden
        self.register_buffer("mask", torch.triu(torch.full((1, 1, length, length), float('-inf')), diagonal=1))
        self.register_buffer("positions", torch.arange(length))

    def forward(self, embeddings):
        positions = self.positions.view(1, 1, -1).expand(3, 1, -1)
        rotary = self.model.rotary_emb(embeddings, positions)
        hidden = embeddings
        for layer in self.model.layers:
            hidden = layer(hidden, attention_mask=self.mask, position_ids=positions[0],
                           past_key_values=None, output_attentions=False, use_cache=False,
                           cache_position=self.positions, position_embeddings=rotary)[0]
        hidden = self.model.norm(hidden[:, -1:, :])
        return (self.head(hidden), hidden) if self.with_hidden else self.head(hidden)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('model', type=Path)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--length', type=int, default=16)
    parser.add_argument('--voice-test', action='store_true', help='Capture real VoiceDesign input with explicit female/emotion direction')
    parser.add_argument('--custom-speaker',help='Capture a real CustomVoice prefix without emotion instructions')
    args = parser.parse_args()
    if not 1 <= args.length <= 256:
        parser.error('length must be in [1, 256]')
    torch.set_num_threads(4)
    tts = Qwen3TTSModel.from_pretrained(str(args.model), device_map='cpu',
        dtype=torch.float32, local_files_only=True, attn_implementation='eager')
    talker = tts.model.talker.eval()
    torch.manual_seed(42)
    sample = torch.randn(1, args.length, talker.config.hidden_size)
    with_hidden = args.voice_test or bool(args.custom_speaker)
    if with_hidden:
        class Captured(Exception):
            pass
        captured = {}
        original_generate = talker.generate
        def capture(**kwargs):
            captured.update(kwargs)
            raise Captured()
        talker.generate = capture
        try:
            if args.custom_speaker:
                tts.generate_custom_voice(text="I'm sorry about the charge. I'll fix it for you.",language='English',
                    speaker=args.custom_speaker,non_streaming_mode=False)
            else:
                tts.generate_voice_design(text="I understand. Let's fix this.", language='English',
                    instruct='A natural American English female voice. Speak with quiet empathy, pause after understand, then sound confident and reassuring.',
                    non_streaming_mode=False)
        except Captured:
            pass
        finally:
            talker.generate = original_generate
        sample = captured['inputs_embeds'].detach()
        args.length = sample.shape[1]
        print(json.dumps({'voice_design_prefix_length':args.length}),flush=True)
    wrapper = Prefill(talker, args.length, with_hidden=with_hidden).eval()
    with torch.inference_mode():
        reference = talker.codec_head(talker.model(inputs_embeds=sample, use_cache=False).last_hidden_state[:, -1:, :])
        source.apply_multimodal_rotary_pos_emb = text_rotary
        actual = wrapper(sample)
        if with_hidden:
            actual = actual[0]
        error = (reference - actual).abs().max().item()
        torch.testing.assert_close(actual, reference, atol=1e-5, rtol=1e-4)
        print(json.dumps({'static_wrapper_max_abs_error': error}), flush=True)
        traced = torch.jit.trace(wrapper, sample, strict=False, check_trace=False)
    converted = ct.convert(traced, convert_to='mlprogram',
        inputs=[ct.TensorType(name='embeddings', shape=sample.shape, dtype=np.float32)],
        outputs=[ct.TensorType(name=n) for n in (['logits','hidden'] if with_hidden else ['logits'])],
        compute_precision=ct.precision.FLOAT16,
        minimum_deployment_target=ct.target.macOS15)
    converted.short_description = 'Experimental Qwen3-TTS talker prefill only; no KV cache outputs'
    args.output.parent.mkdir(parents=True, exist_ok=True)
    converted.save(args.output)
    if with_hidden:
        np.savez(args.output.with_suffix('.inputs.npz'),embeddings=sample.numpy(),reference_logits=reference.numpy())
    print(f'Saved {args.output}', flush=True)


if __name__ == '__main__':
    main()
