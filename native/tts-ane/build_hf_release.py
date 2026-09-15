"""Assemble the runnable HF folder without copying model bytes on one filesystem."""

import argparse
import hashlib
import json
import os
import shutil
from pathlib import Path

from shared_weights import WEIGHT_RELATIVE_PATH


def link_or_copy(source, target):
    try:
        os.link(source, target)
    except OSError:
        shutil.copy2(source, target)
    return target


def copytree(source, target):
    shutil.copytree(source, target, copy_function=link_or_copy)


def sha256(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def copy_shared_package(source, target, canonical_digest):
    weight = source / WEIGHT_RELATIVE_PATH
    if not weight.is_file() or sha256(weight) != canonical_digest:
        raise ValueError(f"Shared talker weights differ: {source}")
    parent = weight.parent
    shutil.copytree(
        source,
        target,
        copy_function=link_or_copy,
        ignore=lambda directory, names: [weight.name] if Path(directory) == parent else [],
    )


def validate_model_config(output):
    config = json.loads((output / "model-config.json").read_text())
    missing = [path for path in config["components"].values() if not (output / path).exists()]
    shared = config["shared_weight_blob"]
    if missing or not (output / shared["canonical"]).is_file():
        raise ValueError(f"Invalid model component paths: {missing}")
    unexpected = [path for path in shared["consumers"] if (output / path).exists()]
    if unexpected:
        raise ValueError(f"Duplicate shared weights remain: {unexpected}")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("output", type=Path)
    parser.add_argument("--models", type=Path, default=Path("models"))
    parser.add_argument("--sample", type=Path, required=True)
    args = parser.parse_args()
    root = Path(__file__).resolve().parent
    if args.output.exists():
        parser.error("Output exists")
    args.output.mkdir(parents=True)
    files = [
        "qwen3_tts_ane.py",
        "example.py",
        "verify_install.py",
        "voice_stream.py",
        "runtime_frontend.py",
        "shared_weights.py",
        "text_parts.py",
    ]
    for name in files:
        shutil.copy2(root / name, args.output / name)
    shutil.copy2(root / "requirements-model.txt", args.output / "requirements.txt")
    shutil.copy2(root / "MODEL_CONFIG.json", args.output / "model-config.json")
    shutil.copy2(root / "MODEL_CARD.md", args.output / "README.md")
    shutil.copy2(root / "LICENSE", args.output / "LICENSE")
    shutil.copy2(root / "HF_GITATTRIBUTES", args.output / ".gitattributes")
    copytree(args.models / "qwen06-runtime-assets-hybrid", args.output / "frontend")
    destination = args.output / "models"
    destination.mkdir()
    talker = args.models / "qwen06-stateful-fp16" / "qwen06_cached_block0.mlpackage"
    prefill = args.models / "qwen06-full-fp16" / "qwen06_prefill_block0.mlpackage"
    long_talker = args.models / "qwen06-long512-fp16.mlpackage"
    selected = {
        talker: destination / "talker" / "qwen06_cached_block0.mlpackage",
        prefill: destination / "prefill" / "qwen06_prefill_block0.mlpackage",
        args.models / "qwen06-outlier256-w8-safe-down.mlpackage": destination
        / "qwen06-outlier256-w8-safe-down.mlpackage",
        args.models / "streaming-decoder-explicit-noslice-fp16.mlpackage": destination
        / "streaming-decoder-explicit-noslice-fp16.mlpackage",
        args.models / "qwen06-text-projection32.mlpackage": destination
        / "qwen06-text-projection32.mlpackage",
        long_talker: destination / "qwen06-long512-fp16.mlpackage",
    }
    canonical_digest = sha256(talker / WEIGHT_RELATIVE_PATH)
    for source, target in selected.items():
        target.parent.mkdir(parents=True, exist_ok=True)
        if source in {prefill, long_talker}:
            copy_shared_package(source, target, canonical_digest)
        else:
            copytree(source, target)
    samples = args.output / "samples"
    samples.mkdir()
    shutil.copy2(args.sample, samples / "serena.wav")
    validate_model_config(args.output)
    payloads = sorted(path for path in args.output.rglob("*") if path.is_file())
    checksums = "".join(f"{sha256(path)}  {path.relative_to(args.output)}\n" for path in payloads)
    (args.output / "SHA256SUMS").write_text(checksums)
    manifest = {
        str(path.relative_to(args.output)): {"bytes": path.stat().st_size, "sha256": sha256(path)}
        for path in args.output.rglob("*")
        if path.is_file()
    }
    (args.output / "release-manifest.json").write_text(
        json.dumps(
            {"files": manifest, "total_bytes": sum(item["bytes"] for item in manifest.values())},
            indent=2,
        )
        + "\n"
    )
    print(
        json.dumps(
            {
                "files": len(manifest),
                "bytes": sum(item["bytes"] for item in manifest.values()),
                "output": str(args.output),
            }
        )
    )


if __name__ == "__main__":
    main()
