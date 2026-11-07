#!/usr/bin/env python3
"""One version, one ABI, everywhere -- checked before a release, not after.

Reads the four places a release can disagree with itself:

    Cargo.toml                 workspace version
    sdks/python/pyproject.toml Python version
    sdks/nodejs/package.json   npm version
    omni.h / _ffi.py / bindings.js   ABI level

    python scripts/check-versions.py
    python scripts/check-versions.py --tag v0.1.0

Exits non-zero on any mismatch. The release workflow runs it before it builds
anything, and `npm publish` runs an equivalent check in scripts/prepack.js, so
a tarball cannot be published under a version the repository does not agree on.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def capture(pattern: str, path: Path, what: str) -> str:
    match = re.search(pattern, read(path), re.MULTILINE)
    if match is None:
        raise SystemExit(f"check-versions: could not read {what} from {path}")
    return match.group(1)


def versions() -> dict[str, str]:
    return {
        "Cargo.toml": capture(
            r"^\[workspace\.package\][\s\S]*?^version\s*=\s*\"([^\"]+)\"",
            ROOT / "Cargo.toml",
            "workspace version",
        ),
        "pyproject.toml": capture(
            r"^\[project\][\s\S]*?^version\s*=\s*\"([^\"]+)\"",
            ROOT / "sdks" / "python" / "pyproject.toml",
            "project version",
        ),
        "package.json": json.loads(read(ROOT / "sdks" / "nodejs" / "package.json"))[
            "version"
        ],
    }


def abi_levels() -> dict[str, str]:
    return {
        "omni.h": capture(
            r"^#define\s+OMNI_ABI_VERSION\s+(\d+)",
            ROOT / "crates" / "omni-ffi" / "include" / "omni.h",
            "OMNI_ABI_VERSION",
        ),
        "Python": capture(
            r"^OMNI_ABI_VERSION\s*=\s*(\d+)",
            ROOT / "sdks" / "python" / "omni" / "_ffi.py",
            "OMNI_ABI_VERSION",
        ),
        "Node": capture(
            r"^const ABI_VERSION\s*=\s*(\d+);",
            ROOT / "sdks" / "nodejs" / "lib" / "bindings.js",
            "ABI_VERSION",
        ),
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--tag", help="release tag to match, e.g. v0.1.0")
    args = parser.parse_args()

    problems: list[str] = []

    found = versions()
    if len(set(found.values())) != 1:
        problems.append(
            "version mismatch: " + ", ".join(f"{k} {v}" for k, v in found.items())
        )
    if args.tag:
        tag = args.tag.removeprefix("v")
        if tag != found["Cargo.toml"]:
            problems.append(
                f"tag {args.tag} names {tag}, but the packages say {found['Cargo.toml']}"
            )

    abi = abi_levels()
    if len(set(abi.values())) != 1:
        problems.append("ABI mismatch: " + ", ".join(f"{k} {v}" for k, v in abi.items()))

    for problem in problems:
        print(f"check-versions: {problem}", file=sys.stderr)
    if problems:
        return 1

    print(
        "check-versions: version "
        + found["Cargo.toml"]
        + (f" (tag {args.tag})" if args.tag else "")
        + ", ABI "
        + abi["omni.h"]
        + " -- every surface agrees"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
