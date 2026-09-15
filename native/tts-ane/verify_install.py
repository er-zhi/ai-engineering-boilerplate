"""Validate a downloaded model bundle before the first expensive Core ML load."""

import argparse
import hashlib
import importlib.util
import json
import platform
import sys
from pathlib import Path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--checksums", action="store_true", help="Hash every file; slower but detects corruption"
    )
    args = parser.parse_args()
    root = Path(__file__).resolve().parent
    errors = []
    if platform.system() != "Darwin" or platform.machine() != "arm64":
        errors.append("Apple Silicon macOS is required")
    if not ((3, 10) <= sys.version_info[:2] <= (3, 13)):
        errors.append("Python 3.10 through 3.13 is required")
    for dependency in ("coremltools", "numpy", "tokenizers"):
        if importlib.util.find_spec(dependency) is None:
            errors.append(f"missing dependency: {dependency}")
    config = json.loads((root / "model-config.json").read_text())
    missing = [path for path in config["components"].values() if not (root / path).exists()]
    if missing:
        errors.append(f"missing components: {missing}")
    checked = 0
    if args.checksums:
        for line in (root / "SHA256SUMS").read_text().splitlines():
            expected, relative = line.split("  ", 1)
            path = root / relative
            if not path.is_file():
                errors.append(f"missing checksum target: {relative}")
                continue
            with path.open("rb") as source:
                actual = hashlib.file_digest(source, "sha256").hexdigest()
            if actual != expected:
                errors.append(f"checksum mismatch: {relative}")
            checked += 1
    report = {
        "status": "PASS" if not errors else "FAIL",
        "python": platform.python_version(),
        "platform": platform.platform(),
        "components": len(config["components"]),
        "checksums_verified": checked,
        "errors": errors,
    }
    print(json.dumps(report, indent=2))
    if errors:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
