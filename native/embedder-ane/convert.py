"""Rebuilds models/embeddinggemma-300m-8bit.mlpackage and models/tokenizer.json from source weights."""

import math

import coremltools as ct
import numpy as np
import torch
from huggingface_hub import hf_hub_download
from sentence_transformers import SentenceTransformer

from config import DOCUMENT_PREFIX, QUERY_PREFIX, SEQ_LEN

MODEL_NAME = "unsloth/embeddinggemma-300m"

HIDDEN_SIZE = 768
N_HEADS = 3
N_KV_HEADS = 1
HEAD_DIM = 256
N_REP = N_HEADS // N_KV_HEADS
QUERY_PRE_ATTN_SCALAR = 256
ROPE_THETA_GLOBAL = 1_000_000.0
ROPE_THETA_LOCAL = 10_000.0
SLIDING_WINDOW = 257
LAYER_TYPES = (["sliding_attention"] * 5 + ["full_attention"]) * 4


def rotate_half(x):
    x1, x2 = x.chunk(2, dim=-1)
    return torch.cat((-x2, x1), dim=-1)


def build_rope(theta, seq_len, head_dim):
    inv_freq = 1.0 / (theta ** (torch.arange(0, head_dim, 2, dtype=torch.float32) / head_dim))
    positions = torch.arange(seq_len, dtype=torch.float32)
    freqs = torch.outer(positions, inv_freq)
    emb = torch.cat((freqs, freqs), dim=-1)
    return emb.cos(), emb.sin()


class ManualGemma3Attention(torch.nn.Module):
    def __init__(self, hf_attn):
        super().__init__()
        self.q_proj = hf_attn.q_proj
        self.k_proj = hf_attn.k_proj
        self.v_proj = hf_attn.v_proj
        self.o_proj = hf_attn.o_proj
        self.q_norm = hf_attn.q_norm
        self.k_norm = hf_attn.k_norm

    def forward(self, hidden, cos, sin, mask):
        batch, seq_len, _ = hidden.shape

        def split_heads(x, n_heads):
            return x.view(batch, seq_len, n_heads, HEAD_DIM).transpose(1, 2)

        q = split_heads(self.q_proj(hidden), N_HEADS)
        k = split_heads(self.k_proj(hidden), N_KV_HEADS)
        v = split_heads(self.v_proj(hidden), N_KV_HEADS)

        q = self.q_norm(q)
        k = self.k_norm(k)

        cos_b = cos.unsqueeze(0).unsqueeze(0)
        sin_b = sin.unsqueeze(0).unsqueeze(0)
        q = q * cos_b + rotate_half(q) * sin_b
        k = k * cos_b + rotate_half(k) * sin_b

        k = k.repeat_interleave(N_REP, dim=1)
        v = v.repeat_interleave(N_REP, dim=1)

        scores = torch.matmul(q, k.transpose(-1, -2)) / math.sqrt(QUERY_PRE_ATTN_SCALAR)
        scores = scores + mask
        weights = torch.softmax(scores, dim=-1)
        context = torch.matmul(weights, v)
        context = context.transpose(1, 2).contiguous().view(batch, seq_len, N_HEADS * HEAD_DIM)
        return self.o_proj(context)


class ManualGemma3Layer(torch.nn.Module):
    def __init__(self, hf_layer):
        super().__init__()
        self.attention = ManualGemma3Attention(hf_layer.self_attn)
        self.input_layernorm = hf_layer.input_layernorm
        self.post_attention_layernorm = hf_layer.post_attention_layernorm
        self.pre_feedforward_layernorm = hf_layer.pre_feedforward_layernorm
        self.post_feedforward_layernorm = hf_layer.post_feedforward_layernorm
        self.gate_proj = hf_layer.mlp.gate_proj
        self.up_proj = hf_layer.mlp.up_proj
        self.down_proj = hf_layer.mlp.down_proj
        self.act_fn = hf_layer.mlp.act_fn

    def forward(self, hidden, cos, sin, mask):
        residual = hidden
        hidden = self.input_layernorm(hidden)
        hidden = self.attention(hidden, cos, sin, mask)
        hidden = self.post_attention_layernorm(hidden)
        hidden = residual + hidden

        residual = hidden
        hidden = self.pre_feedforward_layernorm(hidden)
        hidden = self.down_proj(self.act_fn(self.gate_proj(hidden)) * self.up_proj(hidden))
        hidden = self.post_feedforward_layernorm(hidden)
        hidden = residual + hidden
        return hidden


class ManualGemma3Encoder(torch.nn.Module):
    def __init__(self, hf_model):
        super().__init__()
        self.embed_tokens = hf_model.embed_tokens
        self.layers = torch.nn.ModuleList([ManualGemma3Layer(layer) for layer in hf_model.layers])
        self.final_norm = hf_model.norm

        global_cos, global_sin = build_rope(ROPE_THETA_GLOBAL, SEQ_LEN, HEAD_DIM)
        local_cos, local_sin = build_rope(ROPE_THETA_LOCAL, SEQ_LEN, HEAD_DIM)
        self.register_buffer("global_cos", global_cos, persistent=False)
        self.register_buffer("global_sin", global_sin, persistent=False)
        self.register_buffer("local_cos", local_cos, persistent=False)
        self.register_buffer("local_sin", local_sin, persistent=False)

        positions = torch.arange(SEQ_LEN)
        distance = (positions.view(-1, 1) - positions.view(1, -1)).abs()
        window_open = (distance < SLIDING_WINDOW).float()
        window_mask = (1.0 - window_open) * -10000.0
        self.register_buffer("window_mask", window_mask, persistent=False)

    def forward(self, input_ids, attention_mask):
        hidden = self.embed_tokens(input_ids)

        pad_mask = (1.0 - attention_mask.view(attention_mask.size(0), 1, 1, -1).float()) * -10000.0
        full_mask = pad_mask
        sliding_mask = pad_mask + self.window_mask.unsqueeze(0).unsqueeze(0)

        for layer, layer_type in zip(self.layers, LAYER_TYPES):
            if layer_type == "sliding_attention":
                cos, sin, mask = self.local_cos, self.local_sin, sliding_mask
            else:
                cos, sin, mask = self.global_cos, self.global_sin, full_mask
            hidden = layer(hidden, cos, sin, mask)

        return self.final_norm(hidden)


class Wrapper(torch.nn.Module):
    def __init__(self, st_model):
        super().__init__()
        self.backbone = ManualGemma3Encoder(st_model[0].auto_model)
        self.dense1 = st_model[2].linear
        self.dense2 = st_model[3].linear
        assert self.dense1.in_features == HIDDEN_SIZE, (
            f"pooled vector width {self.dense1.in_features} != HIDDEN_SIZE {HIDDEN_SIZE} "
            "— the backbone's hidden size assumption no longer matches the loaded model"
        )

    def forward(self, input_ids, attention_mask):
        token_embeddings = self.backbone(input_ids, attention_mask)
        mask = attention_mask.unsqueeze(-1).expand(token_embeddings.size()).float()
        summed = torch.sum(token_embeddings * mask, dim=1)
        counts = torch.clamp(mask.sum(dim=1), min=1e-9)
        pooled = summed / counts
        projected = self.dense2(self.dense1(pooled))
        return torch.nn.functional.normalize(projected, p=2, dim=1)


def verify_one(wrapper, st_model, tokenizer, label, prefix, text, prompt_name):
    encoded = tokenizer(
        prefix + text, padding="max_length", truncation=True, max_length=SEQ_LEN, return_tensors="pt"
    )
    with torch.no_grad():
        wrapper_out = wrapper(encoded["input_ids"].long(), encoded["attention_mask"].long())
    reference = st_model.encode([text], prompt_name=prompt_name, normalize_embeddings=True)
    cos = float(np.dot(wrapper_out[0].numpy(), reference[0]) / (
        np.linalg.norm(wrapper_out[0].numpy()) * np.linalg.norm(reference[0])
    ))
    print(f"cos(manual attention, SentenceTransformer.encode) [{label}] = {cos:.6f}")
    if cos < 0.999:
        raise SystemExit(f"Manual attention rewrite diverges from reference on {label}: cos={cos}")


def verify(wrapper, st_model, tokenizer):
    # Both task prefixes are checked independently: they route through different
    # SentenceTransformer prompt names, so a document-path prefix mistake would not be caught by
    # only verifying the query path.
    verify_one(wrapper, st_model, tokenizer, "query", QUERY_PREFIX, "how do I use claude code", "query")
    verify_one(
        wrapper,
        st_model,
        tokenizer,
        "document",
        DOCUMENT_PREFIX,
        "Claude Code is an agentic coding tool that runs in your terminal.",
        "document",
    )


def main():
    print(f"Loading {MODEL_NAME}...")
    st_model = SentenceTransformer(MODEL_NAME, device="cpu")
    st_model.eval()

    wrapper = Wrapper(st_model)
    wrapper.eval()
    verify(wrapper, st_model, st_model.tokenizer)

    dummy_ids = torch.randint(0, 1000, (1, SEQ_LEN), dtype=torch.int32).long()
    dummy_mask = torch.ones((1, SEQ_LEN), dtype=torch.int32).long()

    print("Tracing...")
    traced = torch.jit.trace(wrapper, (dummy_ids, dummy_mask))

    print("Converting to Core ML (fixed shape, fp16)...")
    mlmodel = ct.convert(
        traced,
        convert_to="mlprogram",
        inputs=[
            ct.TensorType(name="input_ids", shape=(1, SEQ_LEN), dtype=np.int32),
            ct.TensorType(name="attention_mask", shape=(1, SEQ_LEN), dtype=np.int32),
        ],
        outputs=[ct.TensorType(name="embedding")],
        compute_precision=ct.precision.FLOAT16,
        minimum_deployment_target=ct.target.macOS15,
        compute_units=ct.ComputeUnit.CPU_AND_NE,
    )

    print("Quantizing to 8-bit...")
    from coremltools.optimize.coreml import (
        OpLinearQuantizerConfig,
        OptimizationConfig,
        linear_quantize_weights,
    )

    op_config = OpLinearQuantizerConfig(mode="linear_symmetric", weight_threshold=512)
    config = OptimizationConfig(global_config=op_config)
    quantized = linear_quantize_weights(mlmodel, config=config)
    quantized.save("models/embeddinggemma-300m-8bit.mlpackage")
    print("Saved models/embeddinggemma-300m-8bit.mlpackage")

    tokenizer_path = hf_hub_download(MODEL_NAME, "tokenizer.json")
    with open(tokenizer_path, "rb") as source, open("models/tokenizer.json", "wb") as destination:
        destination.write(source.read())
    print("Saved models/tokenizer.json")


if __name__ == "__main__":
    main()
