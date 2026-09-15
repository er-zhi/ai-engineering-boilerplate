"""Download the pinned, ready-to-run Qwen3-TTS ANE release from Hugging Face."""

import argparse
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parent
SOURCE_FILE = ROOT / "MODEL_SOURCE.json"


def read_source(path=SOURCE_FILE):
    source = json.loads(Path(path).read_text())
    required = {"repo_id", "revision", "source_repository"}
    missing = required - source.keys()
    if missing:
        raise ValueError(f"Model source is missing fields: {sorted(missing)}")
    return source


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=ROOT / "model")
    parser.add_argument(
        "--revision",
        help="Override the pinned immutable HF revision (for explicit upgrades only)",
    )
    parser.add_argument("--show-source", action="store_true")
    args = parser.parse_args()
    source = read_source()
    revision = args.revision or source["revision"]
    if args.show_source:
        print(json.dumps({**source, "revision": revision}, indent=2))
        return

    from huggingface_hub import snapshot_download

    destination = snapshot_download(
        repo_id=source["repo_id"],
        revision=revision,
        local_dir=args.output,
    )
    root = Path(destination)
    required = ("model-config.json", "requirements.txt", "SHA256SUMS", "example.py")
    missing = [name for name in required if not (root / name).is_file()]
    if missing:
        raise RuntimeError(f"Downloaded release is incomplete: {missing}")
    print(root)


if __name__ == "__main__":
    main()
