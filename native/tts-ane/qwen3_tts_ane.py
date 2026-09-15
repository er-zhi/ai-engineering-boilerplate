"""Embeddable Serena streaming API backed by Apple Neural Engine Core ML graphs."""

import json
import platform
from dataclasses import dataclass
from pathlib import Path

from shared_weights import WEIGHT_RELATIVE_PATH, materialize_shared_package
from voice_stream import VoiceStream

SAMPLE_RATE = 24000
CHANNELS = 1
MAX_FRAMES = 502
MAX_TEXT_CHARACTERS = 32768


@dataclass(frozen=True)
class AudioChunk:
    pcm_s16le: bytes
    sequence: int
    metadata: dict
    sample_rate: int = SAMPLE_RATE
    channels: int = CHANNELS


class Qwen3TTSANE:
    def __init__(self, root=None, cache=None, gate=None):
        if platform.system() != "Darwin" or platform.machine() != "arm64":
            raise RuntimeError("Qwen3TTSANE requires Apple Silicon macOS")
        self.root = Path(root or Path(__file__).resolve().parent).resolve()
        self.cache = Path(cache or "~/Library/Caches/Qwen3TTSANE").expanduser().resolve()
        gate = Path(gate).resolve() if gate is not None else None
        config_path = self.root / "model-config.json"
        try:
            self.config = json.loads(config_path.read_text())
            components = self.config["components"]
            if self.config["schema_version"] != 1:
                raise ValueError("unsupported schema version")
        except (OSError, KeyError, TypeError, ValueError, json.JSONDecodeError) as error:
            raise RuntimeError(f"Invalid model configuration: {config_path}") from error
        required = {
            "talker",
            "prefill",
            "long_talker",
            "predictor",
            "decoder",
            "text_projection",
            "frontend",
        }
        missing = required - set(components)
        if missing:
            raise RuntimeError(f"Model configuration is missing components: {sorted(missing)}")
        resolved = {name: (self.root / path).resolve() for name, path in components.items()}
        if any(self.root not in path.parents for path in resolved.values()):
            raise RuntimeError("Model component path escapes the bundle root")
        talker = resolved["talker"]
        weight = talker / WEIGHT_RELATIVE_PATH
        prefill, prefill_mode = materialize_shared_package(
            resolved["prefill"], weight, self.cache / "packages" / "prefill"
        )
        long_talker, long_mode = materialize_shared_package(
            resolved["long_talker"], weight, self.cache / "packages" / "long-talker"
        )
        modes = {
            "prefill": prefill_mode,
            "long_talker": long_mode,
        }
        self.shared_weight_modes = modes
        self._voice = VoiceStream(
            resolved["frontend"],
            talker.parent,
            gate,
            "I'm ready.",
            "",
            compiled_dir=self.cache / "compiled",
            predictor_package=resolved["predictor"],
            prefill_packages=prefill.parent,
            speaker="Serena",
            model_prefix="qwen06",
            block_count=1,
            decoder_package=resolved["decoder"],
            experimental_history_decoder=True,
            text_projection_package=resolved["text_projection"],
            long_talker_package=long_talker,
            frontend_assets=resolved["frontend"],
        )

    def stream(self, text, max_frames=MAX_FRAMES):
        if not isinstance(text, str) or not text.strip() or len(text) > MAX_TEXT_CHARACTERS:
            raise ValueError(
                f"text must contain 1..{MAX_TEXT_CHARACTERS} characters and not be blank"
            )
        if (
            not isinstance(max_frames, int)
            or isinstance(max_frames, bool)
            or not 1 <= max_frames <= MAX_FRAMES
        ):
            raise ValueError(f"max_frames must be 1..{MAX_FRAMES}")
        for sequence, (pcm, metadata) in enumerate(self._voice.chunks_for_text(text, max_frames)):
            yield AudioChunk(pcm, sequence, metadata)

    def synthesize(self, text, max_frames=MAX_FRAMES):
        return b"".join(chunk.pcm_s16le for chunk in self.stream(text, max_frames))
