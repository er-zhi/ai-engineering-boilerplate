"""Export one Qwen3-TTS 12-Hz codec frame to a fixed Core ML graph.

The generated package is intentionally kept outside git.  A single codec frame
decodes to 1,920 samples (80 ms at 24 kHz). It resets causal history and is NOT
a continuous streaming decoder. This exporter is a feasibility probe: the
ANE gate, not successful conversion alone, decides whether the graph is usable.
"""

from __future__ import annotations

import argparse
from pathlib import Path

import coremltools as ct
import numpy as np
import torch

from qwen_tts.core.tokenizer_12hz.modeling_qwen3_tts_tokenizer_v2 import (
    Qwen3TTSTokenizerV2CausalTransConvNet,
    Qwen3TTSTokenizerV2Model,
)


class PhaseUpsampleConv(torch.nn.Module):
    """Rewrite Qwen's cropped ConvTranspose1d as 1x1 conv + phase reshape.

    Qwen uses kernel=stride for the first two upsamplers and kernel=2*stride
    in the decoder. In both cases each output phase depends only on the
    current and previous input frame, so a shifted concatenation plus ordinary
    convolution is algebraically identical and avoids Core ML ConvTranspose.
    """

    def __init__(self, source: torch.nn.ConvTranspose1d) -> None:
        super().__init__()
        if source.kernel_size[0] not in (source.stride[0], 2 * source.stride[0]) or source.groups != 1:
            raise ValueError("unsupported ConvTranspose shape")
        self.factor = int(source.stride[0])
        self.out_channels = int(source.out_channels)
        self.in_channels = int(source.in_channels)
        kernel = int(source.kernel_size[0])
        self.has_previous = kernel == 2 * self.factor
        self.current = torch.nn.Conv1d(
            source.in_channels,
            source.out_channels * self.factor,
            kernel_size=1,
            bias=True,
        )
        self.previous = torch.nn.Conv1d(
            source.in_channels,
            source.out_channels * self.factor,
            kernel_size=1,
            bias=False,
        )
        with torch.no_grad():
            current = torch.zeros_like(self.current.weight)
            previous = torch.zeros_like(self.previous.weight)
            for phase in range(self.factor):
                indices = torch.arange(self.out_channels) * self.factor + phase
                current[indices, :, 0] = source.weight[:, :, phase].T
                if kernel == 2 * self.factor:
                    previous[indices, :, 0] = source.weight[:, :, phase + self.factor].T
            self.current.weight.copy_(current)
            self.previous.weight.copy_(previous)
            self.current.bias.copy_(source.bias[:, None].repeat(1, self.factor).reshape(-1))

    def forward(self, hidden_state: torch.Tensor) -> torch.Tensor:
        previous = torch.nn.functional.pad(hidden_state, (1, 0))[..., :-1]
        y = self.current(hidden_state) + self.previous(previous)
        batch, _, steps = y.shape
        y = y.reshape(batch, self.out_channels, self.factor, steps)
        return y.permute(0, 1, 3, 2).reshape(batch, self.out_channels, steps * self.factor)


def replace_nonoverlap_transconvs(module: torch.nn.Module) -> int:
    replaced = 0
    for name, child in list(module.named_children()):
        if isinstance(child, Qwen3TTSTokenizerV2CausalTransConvNet):
            conv = child.conv
            if conv.kernel_size[0] in (conv.stride[0], 2 * conv.stride[0]) and conv.groups == 1:
                setattr(module, name, PhaseUpsampleConv(conv))
                replaced += 1
                continue
        replaced += replace_nonoverlap_transconvs(child)
    return replaced


class OneFrameDecoder(torch.nn.Module):
    def __init__(self, tokenizer: Qwen3TTSTokenizerV2Model, latent_input: bool = False, tail_only: bool = False, tail_frames: int = 1) -> None:
        super().__init__()
        self.decoder = tokenizer.decoder
        self.latent_input = latent_input
        self.tail_only = tail_only
        if tail_only:
            stages = [block for group in self.decoder.upsample for block in group]
            for block in self.decoder.decoder:
                stages.extend(list(block.block) if hasattr(block,'block') else [block])
            self.tail_stages = torch.nn.ModuleList(stages)
            required = 1920 * tail_frames
            outputs = []
            for block in reversed(stages):
                outputs.append(required)
                name = type(block).__name__
                if isinstance(block,PhaseUpsampleConv):
                    required = (required + block.factor - 1)//block.factor + int(block.has_previous)
                elif name == 'Qwen3TTSTokenizerV2ConvNeXtBlock':
                    required += 6
                elif name == 'Qwen3TTSTokenizerV2DecoderDecoderResidualUnit':
                    required += 6 * block.conv1.conv.dilation[0]
                elif name == 'Qwen3TTSTokenizerV2CausalConvNet':
                    if block.conv.stride != (1,):
                        raise ValueError('Unsupported strided causal convolution')
                    required += (block.conv.kernel_size[0]-1)*block.conv.dilation[0]
                elif name != 'SnakeBeta':
                    raise ValueError(f'Unsupported tail stage: {name}')
            self.tail_required = required
            self.tail_output_lengths = list(reversed(outputs))

    def forward(self, codes: torch.Tensor) -> torch.Tensor:
        # The tokenizer's public decode path uses [B, Q, T].  Avoid its
        # dynamic chunk loop: the ANE graph is deliberately one frame wide.
        hidden = codes if self.latent_input else self.decoder.quantizer.decode(codes)
        # RVQ statistics are stored as fp32 buffers even when the checkpoint is
        # loaded in fp16; make the convolution contract explicit for Core ML.
        hidden = hidden.to(dtype=self.decoder.pre_conv.conv.weight.dtype)
        hidden = self.decoder.pre_conv(hidden).transpose(1, 2)
        steps = hidden.shape[1]
        # Transformers 4.57 builds this mask through a functorch/vmap helper,
        # which is not traceable. A fixed additive mask is equivalent here.
        mask = torch.triu(torch.full((1, 1, steps, steps), float('-inf'), dtype=hidden.dtype, device=hidden.device), diagonal=1)
        position_ids = torch.arange(steps, device=hidden.device).unsqueeze(0)
        cache_position = torch.arange(steps, device=hidden.device)
        masks = {"full_attention": mask, "sliding_attention": mask}
        hidden = self.decoder.pre_transformer(
            inputs_embeds=hidden,
            attention_mask=masks,
            position_ids=position_ids,
            cache_position=cache_position,
            use_cache=False,
        ).last_hidden_state
        hidden = hidden.permute(0, 2, 1)
        if self.tail_only:
            hidden = hidden[..., -self.tail_required:]
            for block,length in zip(self.tail_stages,self.tail_output_lengths):
                hidden = block(hidden)[..., -length:]
            return hidden.clamp(min=-1,max=1).squeeze(1)
        for blocks in self.decoder.upsample:
            for block in blocks:
                hidden = block(hidden)
        wav = hidden
        for block in self.decoder.decoder:
            wav = block(wav)
        return wav.clamp(min=-1, max=1).squeeze(1)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("model", type=Path, help="Qwen model directory")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--phase-upsample", action="store_true")
    parser.add_argument("--frames", type=int, default=1)
    parser.add_argument('--tail-only',action='store_true',help='Compute only the final frame, retaining the required convolution history')
    parser.add_argument('--tail-frames',type=int,default=1,help='Number of final frames retained by a tail-only graph')
    parser.add_argument(
        "--latent-input",
        action="store_true",
        help="Export the ANE-heavy latent-to-PCM graph; host performs RVQ lookup",
    )
    args = parser.parse_args()
    if not 1 <= args.frames <= 64:
        parser.error('frames must be 1..64; this stays within the decoder attention window')
    if not 1 <= args.tail_frames <= args.frames:
        parser.error('tail-frames must be between 1 and frames')

    tokenizer_path = args.model / "speech_tokenizer"
    tokenizer = Qwen3TTSTokenizerV2Model.from_pretrained(
        tokenizer_path,
        torch_dtype=torch.float32,
        low_cpu_mem_usage=True,
        local_files_only=True,
    ).to(dtype=torch.float32).eval()
    replaced = replace_nonoverlap_transconvs(tokenizer.decoder) if args.phase_upsample else 0
    print(f"replaced non-overlapping ConvTranspose1d layers: {replaced}")
    if args.tail_only and not args.phase_upsample:
        parser.error('tail-only requires phase-upsample')
    wrapper = OneFrameDecoder(tokenizer, latent_input=args.latent_input,tail_only=args.tail_only,tail_frames=args.tail_frames).eval()
    codes = torch.zeros(
        (1, 512, args.frames) if args.latent_input else (1, 16, args.frames),
        dtype=torch.float32 if args.latent_input else torch.long,
    )
    with torch.inference_mode():
        if args.tail_only:
            full = OneFrameDecoder(tokenizer,latent_input=args.latent_input).eval()
            torch.manual_seed(42)
            for _ in range(3):
                probe = tokenizer.decoder.quantizer.decode(torch.randint(0,2048,(1,16,args.frames)))
                expected = full(probe)[..., -1920*args.tail_frames:]
                actual = wrapper(probe)
                torch.testing.assert_close(actual,expected,atol=1e-5,rtol=1e-4)
            print(f'Tail crop FP32 parity PASS; required latent frames={wrapper.tail_required}',flush=True)
        traced = torch.jit.trace(wrapper, (codes,), strict=False)
        reference = wrapper(codes)
    print(f"reference shape={tuple(reference.shape)} range=[{reference.min().item():.4f}, {reference.max().item():.4f}]")

    args.output.parent.mkdir(parents=True, exist_ok=True)
    mlmodel = ct.convert(
        traced,
        convert_to="mlprogram",
        inputs=[
            ct.TensorType(
                name="codes" if not args.latent_input else "latent",
                shape=codes.shape,
                dtype=np.float32 if args.latent_input else np.int32,
            )
        ],
        outputs=[ct.TensorType(name="pcm")],
        compute_precision=ct.precision.FLOAT16,
        minimum_deployment_target=ct.target.macOS15,
    )
    mlmodel.author = "Qwen3-TTS ANE feasibility probe"
    mlmodel.short_description = "Fixed one-frame Qwen3-TTS 12-Hz decoder"
    mlmodel.save(args.output)
    print(f"saved {args.output}")


if __name__ == "__main__":
    main()
