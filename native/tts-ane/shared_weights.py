"""Materializes Core ML packages that share one immutable weight blob."""

import errno
import hashlib
import os
import shutil
from pathlib import Path

WEIGHT_RELATIVE_PATH = Path("Data/com.apple.CoreML/weights/weight.bin")
TALKER_RELATIVE_PATH = Path("talker/qwen06_cached_block0.mlpackage")
PREFILL_RELATIVE_PATH = Path("prefill/qwen06_prefill_block0.mlpackage")
LONG_RELATIVE_PATH = Path("qwen06-long512-fp16.mlpackage")


def package_signature(package, weight):
    digest = hashlib.sha256()
    for path in sorted(package.rglob("*")):
        if path.is_file() and path.relative_to(package) != WEIGHT_RELATIVE_PATH:
            digest.update(str(path.relative_to(package)).encode())
            with path.open("rb") as source:
                digest.update(hashlib.file_digest(source, "sha256").digest())
    state = weight.stat()
    digest.update(f"{state.st_dev}:{state.st_ino}:{state.st_size}:{state.st_mtime_ns}".encode())
    return digest.hexdigest()[:20]


def link_or_copy(source, target):
    try:
        os.link(source, target)
        return "hard-link"
    except OSError as error:
        if error.errno != errno.EXDEV:
            raise
        shutil.copy2(source, target)
        return "copy"


def materialize_shared_package(package, weight, cache):
    existing = package / WEIGHT_RELATIVE_PATH
    if existing.is_file():
        return package, "packaged"
    if not package.is_dir() or not weight.is_file():
        raise FileNotFoundError(f"Incomplete shared-weight package: {package}")
    cache.mkdir(parents=True, exist_ok=True)
    signature = package_signature(package, weight)
    destination = cache / signature / package.name
    destination_weight = destination / WEIGHT_RELATIVE_PATH
    if destination_weight.is_file():
        return destination, "cached"
    temporary = cache / f".{destination.name}.{os.getpid()}.tmp"
    if temporary.exists():
        shutil.rmtree(temporary)
    shutil.copytree(package, temporary)
    destination_weight = temporary / WEIGHT_RELATIVE_PATH
    destination_weight.parent.mkdir(parents=True, exist_ok=True)
    mode = link_or_copy(weight, destination_weight)
    destination.parent.mkdir(parents=True, exist_ok=True)
    try:
        temporary.rename(destination)
    except FileExistsError:
        shutil.rmtree(temporary)
    if not (destination / WEIGHT_RELATIVE_PATH).is_file():
        raise RuntimeError(f"Failed to materialize shared weights for {package}")
    return destination, mode


def materialize_talker_packages(models, cache):
    talker = models / TALKER_RELATIVE_PATH
    weight = talker / WEIGHT_RELATIVE_PATH
    if not weight.is_file():
        raise FileNotFoundError(f"Missing canonical talker weights: {weight}")
    prefill, prefill_mode = materialize_shared_package(
        models / PREFILL_RELATIVE_PATH, weight, cache
    )
    long_talker, long_mode = materialize_shared_package(models / LONG_RELATIVE_PATH, weight, cache)
    return prefill, long_talker, {"prefill": prefill_mode, "long_talker": long_mode}
