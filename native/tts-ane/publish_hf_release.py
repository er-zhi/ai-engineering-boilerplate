"""Mirror a validated release folder to Hugging Face in one commit."""

import argparse
import os
from pathlib import Path

from huggingface_hub import CommitOperationAdd, CommitOperationDelete, HfApi


def local_files(root):
    return {str(path.relative_to(root)) for path in root.rglob("*") if path.is_file()}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("folder", type=Path)
    parser.add_argument("--repo", default="erjigit17/Qwen3-TTS-0.6B-ANE")
    parser.add_argument("--message", required=True)
    parser.add_argument("--dry-run", action="store_true")
    args = parser.parse_args()
    root = args.folder.resolve(strict=True)
    required = {
        "README.md",
        "SHA256SUMS",
        "release-manifest.json",
        "model-config.json",
        "requirements.txt",
    }
    local = local_files(root)
    transient = sorted(
        path
        for path in local
        if "__pycache__" in Path(path).parts or path.endswith((".pyc", ".pyo"))
    )
    if transient:
        parser.error(f"release folder contains transient Python files: {transient}")
    missing = required - local
    if missing:
        parser.error(f"incomplete release folder; missing {sorted(missing)}")
    token = (
        os.environ.get("HF_TOKEN")
        or os.environ.get("HUGGINGFACE_TOKEN")
        or os.environ.get("HUGGING_FACE_HUB_TOKEN")
    )
    if not token:
        parser.error("HF_TOKEN (or compatible Hugging Face token variable) is required")
    api = HfApi(token=token)
    info = api.repo_info(args.repo, repo_type="model")
    remote = set(api.list_repo_files(args.repo, repo_type="model"))
    removed = sorted(remote - local)
    print(f"local={len(local)} remote={len(remote)} delete={len(removed)}")
    for path in removed:
        print(f"DELETE {path}")
    if args.dry_run:
        return
    operations = [CommitOperationDelete(path_in_repo=path) for path in removed]
    operations.extend(
        CommitOperationAdd(path_in_repo=path, path_or_fileobj=root / path) for path in sorted(local)
    )
    result = api.create_commit(
        args.repo,
        repo_type="model",
        operations=operations,
        commit_message=args.message,
        parent_commit=info.sha,
    )
    print(result.commit_url)


if __name__ == "__main__":
    main()
