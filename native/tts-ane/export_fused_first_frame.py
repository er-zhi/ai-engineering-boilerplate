"""Export a no-prefix-cache talker prefill fused with first-frame code prediction."""

import argparse
import json
from pathlib import Path

import coremltools as ct
import numpy as np
import torch
from export_fused_predictor import Fused
from export_voice_prefill_cache import PrefillCache
from qwen_tts import Qwen3TTSModel


class FusedFirstFrame(torch.nn.Module):
    """Produce all first-frame audio codes and continuation KV in one graph."""

    def __init__(self, talker, length):
        super().__init__()
        self.prefill = PrefillCache(talker, 0, talker.config.num_hidden_layers)
        self.predictor = Fused(talker.code_predictor, 15, float_selection=True)
        self.codec_embedding = talker.get_input_embeddings()
        self.register_buffer("ranks", torch.arange(2048, dtype=torch.float32).view(1, 1, -1))
        self.length = length

    def forward(self, embeddings, cosine, sine, attention_mask):
        logits, hidden, keys, values = self.prefill(embeddings, cosine, sine, attention_mask)
        hidden = hidden[:, -1:]
        scores = logits[:, -1:, :2048]
        winners = (scores == scores.amax(-1, keepdim=True)).to(scores.dtype)
        semantic = 2048 - ((2048 - self.ranks) * winners).amax(-1)
        selector = (self.ranks == semantic.unsqueeze(-1)).to(hidden.dtype)
        first_embedding = selector @ self.codec_embedding.weight[:2048]
        residual = self.predictor(hidden, first_embedding)
        return torch.cat((semantic, residual), dim=-1), hidden, keys, values


def capture_prefix(tts, text, speaker):
    captured = {}

    class Captured(Exception):
        pass

    talker = tts.model.talker
    original = talker.generate

    def capture(**kwargs):
        captured.update(kwargs)
        raise Captured

    talker.generate = capture
    try:
        tts.generate_custom_voice(
            text=text,
            language="English",
            speaker=speaker,
            non_streaming_mode=False,
        )
    except Captured:
        pass
    finally:
        talker.generate = original
    return captured["inputs_embeds"].detach()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--text", default="I'm sorry about the charge. I'll fix it for you.")
    parser.add_argument("--speaker", default="Serena")
    parser.add_argument(
        "--expected-codes",
        type=Path,
        help="VoiceStream JSON used to require exact first-frame code parity",
    )
    args = parser.parse_args()
    if args.output.exists():
        parser.error("output already exists; preserve previous candidates")
    torch.set_num_threads(4)
    tts = Qwen3TTSModel.from_pretrained(
        str(args.source),
        dtype=torch.float32,
        device_map="cpu",
        local_files_only=True,
        attn_implementation="eager",
    )
    talker = tts.model.talker.eval()
    sample = capture_prefix(tts, args.text, args.speaker)
    length = sample.shape[1]
    positions = torch.arange(length).reshape(1, 1, -1).expand(3, 1, -1)
    cosine, sine = talker.model.rotary_emb(sample, positions)
    mask = torch.triu(torch.full((1, 1, length, length), float("-inf")), diagonal=1)
    inputs = sample, cosine[0].unsqueeze(1), sine[0].unsqueeze(1), mask
    wrapper = FusedFirstFrame(talker, length).eval()
    with torch.inference_mode():
        outputs = wrapper(*inputs)
        codes = outputs[0].to(torch.int64)
        if args.expected_codes:
            report = json.loads(args.expected_codes.read_text())
            expected = torch.tensor([report["chunks"][0]["codes"]], dtype=torch.int64)
            torch.testing.assert_close(codes, expected, atol=0, rtol=0)
        print(json.dumps({"prefix_tokens": length, "codes": codes.tolist()}), flush=True)
        traced = torch.jit.trace(wrapper, inputs, strict=False, check_trace=False)
    converted = ct.convert(
        traced,
        convert_to="mlprogram",
        inputs=[
            ct.TensorType(name=name, shape=value.shape, dtype=np.float32)
            for name, value in zip(
                ("embeddings", "cosine", "sine", "attention_mask"), inputs, strict=True
            )
        ],
        outputs=[
            ct.TensorType(name=name) for name in ("codes", "hidden", "next_keys", "next_values")
        ],
        compute_precision=ct.precision.FLOAT16,
        minimum_deployment_target=ct.target.macOS15,
        skip_model_load=True,
    )
    converted.short_description = (
        "Qwen3-TTS 0.6B first-frame talker prefill and residual-code predictor"
    )
    args.output.parent.mkdir(parents=True, exist_ok=True)
    converted.save(args.output)
    np.savez(
        args.output.with_suffix(".inputs.npz"),
        **{
            name: value.detach().numpy()
            for name, value in zip(
                ("embeddings", "cosine", "sine", "attention_mask"), inputs, strict=True
            )
        },
        expected_codes=codes.numpy(),
        expected_hidden=outputs[1].numpy(),
        expected_keys=outputs[2].numpy(),
        expected_values=outputs[3].numpy(),
    )
    print(f"Saved candidate: {args.output}", flush=True)


if __name__ == "__main__":
    main()
