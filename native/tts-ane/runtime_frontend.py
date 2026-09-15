"""Minimal Qwen CustomVoice text frontend backed by memory-mapped static arrays."""

import json
from pathlib import Path
from types import SimpleNamespace

import numpy as np
from tokenizers import AddedToken, ByteLevelBPETokenizer


class ArrayEmbedding:
    def __init__(self, path):
        self.array = np.load(path, mmap_mode="r")
        correction = path.with_suffix(".corrections.npz")
        if correction.exists():
            saved = np.load(correction)
            self.flat_index = saved["flat_index"]
            self.value = saved["value"]
        else:
            self.flat_index = np.empty(0, np.int64)
            self.value = np.empty(0, np.float32)

    def __call__(self, indices):
        index = np.asarray(indices)
        output = np.asarray(self.array[index], dtype=np.float32)
        rows = index.reshape(-1)
        values = output.reshape(-1, self.array.shape[1])
        for position, row in enumerate(rows):
            start = int(row) * self.array.shape[1]
            stop = start + self.array.shape[1]
            left = np.searchsorted(self.flat_index, start)
            right = np.searchsorted(self.flat_index, stop)
            if right > left:
                values[position, self.flat_index[left:right] - start] = self.value[left:right]
        return output

    def __getitem__(self, index):
        return self(np.asarray(index))


class NumpyANETextProjection:
    def __init__(self, model, channels, width, output_channels):
        self.model, self.channels, self.width, self.output_channels = (
            model,
            channels,
            width,
            output_channels,
        )

    def __call__(self, embeddings):
        source = np.asarray(embeddings, dtype=np.float32)
        if source.ndim != 3 or source.shape[0] != 1 or source.shape[2] != self.channels:
            raise ValueError("Expected text embeddings [1, tokens, channels]")
        pieces = []
        for offset in range(0, source.shape[1], self.width):
            count = min(self.width, source.shape[1] - offset)
            batch = np.zeros((1, self.channels, 1, self.width), np.float32)
            batch[0, :, 0, :count] = source[0, offset : offset + count].T
            output = self.model.predict({"embeddings": batch})["projected"]
            pieces.append(output[:, :, 0, :count].transpose(0, 2, 1).copy())
        return np.concatenate(pieces, axis=1)


class Talker:
    def __init__(self, assets, metadata, text_projection):
        self.text_embedding = ArrayEmbedding(assets / "text_embeddings.npy")
        self.codec_embedding = ArrayEmbedding(assets / "codec_embedding.npy")
        self.predictor_embeddings = [
            ArrayEmbedding(assets / f"predictor_embedding_{i:02}.npy")
            for i in range(metadata["predictor_embeddings"])
        ]
        self.text_projection = text_projection
        self.config = SimpleNamespace(codec_eos_token_id=metadata["codec_eos_token_id"])

    def get_text_embeddings(self):
        return self.text_embedding

    def get_input_embeddings(self):
        return self.codec_embedding


class RuntimeFrontend:
    def __init__(self, assets, text_projection):
        self.assets = Path(assets)
        self.metadata = json.loads((self.assets / "metadata.json").read_text())
        tokenizer_assets = self.assets / "tokenizer"
        tokenizer_config = json.loads((tokenizer_assets / "tokenizer_config.json").read_text())
        self.tokenizer = ByteLevelBPETokenizer(
            str(tokenizer_assets / "vocab.json"),
            str(tokenizer_assets / "merges.txt"),
            add_prefix_space=False,
            lowercase=False,
        )
        added = [
            AddedToken(
                value["content"],
                single_word=value["single_word"],
                lstrip=value["lstrip"],
                rstrip=value["rstrip"],
                normalized=value["normalized"],
                special=value["special"],
            )
            for value in tokenizer_config["added_tokens_decoder"].values()
        ]
        self.tokenizer.add_special_tokens(added)
        self.model = SimpleNamespace(talker=Talker(self.assets, self.metadata, text_projection))

    def _build_assistant_text(self, text):
        return f"<|im_start|>assistant\n{text}<|im_end|>\n<|im_start|>assistant\n"

    def _tokenize_texts(self, texts):
        return [np.asarray([self.tokenizer.encode(text).ids], np.int64) for text in texts]

    def project_ids(self, ids):
        return self.model.talker.text_projection(
            self.model.talker.get_text_embeddings()(np.asarray([ids]))
        )

    def capture_prompt(self, text, speaker):
        if speaker.lower() != self.metadata["speaker"]:
            raise ValueError(f"This release contains only {self.metadata['speaker']} voice")
        ids = self._tokenize_texts([self._build_assistant_text(text)])[0]
        talker = self.model.talker

        def project(values):
            return talker.text_projection(talker.get_text_embeddings()(values))

        special = np.asarray(
            [
                [
                    self.metadata[name]
                    for name in ("tts_bos_token_id", "tts_eos_token_id", "tts_pad_token_id")
                ]
            ]
        )
        bos, eos, pad = np.split(project(special), 3, axis=1)
        codec_ids = self.metadata["codec_prefix_ids"] + [
            self.metadata["speaker_id"],
            self.metadata["codec_pad_id"],
            self.metadata["codec_bos_id"],
        ]
        codec = talker.get_input_embeddings()(np.asarray([codec_ids]))
        role = project(ids[:, :3])
        prefix = (
            np.concatenate((np.repeat(pad, codec.shape[1] - 2, axis=1), bos), axis=1)
            + codec[:, :-1]
        )
        initial = np.concatenate((role, prefix, project(ids[:, 3:4]) + codec[:, -1:]), axis=1)
        trailing = np.concatenate((project(ids[:, 4:-5]), eos), axis=1)
        return {
            "inputs_embeds": initial.copy(),
            "trailing_text_hidden": trailing.copy(),
            "tts_pad_embed": pad.copy(),
            "repetition_penalty": self.metadata["repetition_penalty"],
            "min_new_tokens": self.metadata["min_new_tokens"],
        }


def load_controls(assets, metadata):
    # Core ML rejects read-only mmap views for inputs; controls are small and writable.
    arrays = {name: np.load(assets / f"{name}.npy") for name in metadata["controls"]}
    groups = []
    for group in metadata["control_groups"]:
        fields = group["fields"]
        length = group["length"]
        groups.append(
            [
                {field: arrays[f"{group['name']}_{field}"][position] for field in fields}
                for position in range(length)
            ]
        )
    return groups


def prepare_runtime_assets(assets, text, speaker, text_projection, frontend_sink):
    assets = Path(assets)
    frontend = RuntimeFrontend(assets, text_projection)
    arrays = frontend.capture_prompt(text, speaker)
    metadata = frontend.metadata
    embeddings = [frontend.model.talker.codec_embedding]
    embeddings += frontend.model.talker.predictor_embeddings
    lookups = [
        np.load(assets / f"lookup_{i:02}.npy", mmap_mode="r") for i in range(metadata["lookups"])
    ]
    if frontend_sink is not None:
        frontend_sink.append(frontend)
    return (
        arrays,
        embeddings,
        lookups,
        load_controls(assets, metadata),
        metadata["codec_eos_token_id"],
    )
