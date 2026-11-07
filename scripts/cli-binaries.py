#!/usr/bin/env python3
"""Stage the `omni` and `omniup` executables for one release platform.

The archives `package.py` builds carry the shared library; this script carries
the two programs a developer actually runs. Each is copied out of the cargo
release directory under the name the update manifest and the docs use, with a
`.sha256` beside it:

    python scripts/cli-binaries.py --target x86_64-pc-windows-msvc --platform win32-x64
    python scripts/cli-binaries.py --skip-build --platform linux-x64

Only the standard library is used, so it runs on a bare CI image.
"""

from __future__ import annotations

import argparse
import hashlib
import re
import shutil
import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

PLATFORMS = ("win32-x64", "linux-x64", "linux-arm64", "darwin-x64", "darwin-arm64")


def host_target() -> str:
    result = subprocess.run(["rustc", "-vV"], check=True, capture_output=True, text=True)
    match = re.search(r"^host:\s*(\S+)$", result.stdout, re.MULTILINE)
    if match is None:
        raise SystemExit("cli-binaries: rustc -vV did not report a host target")
    return match.group(1)


def release_dir(target: str) -> Path:
    # A --target build writes under target/<triple>/release; a plain host build
    # writes under target/release. Check both when they name the same machine,
    # and prefer whichever directory actually holds both executables -- an empty
    # per-target directory can exist from an unrelated earlier build.
    suffix = ".exe" if "windows" in target else ""
    candidates = [ROOT / "target" / target / "release"]
    if target == host_target():
        candidates.append(ROOT / "target" / "release")
    for directory in candidates:
        if all((directory / f"{binary}{suffix}").is_file() for binary in ("omni", "omniup")):
            return directory
    tried = ", ".join(str(directory) for directory in candidates)
    raise SystemExit(
        f"cli-binaries: no release directory holds omni{suffix} and omniup{suffix}.\n"
        f"Tried: {tried}\n"
        f"Build it first: cargo build --release -p omni-cli --target {target}"
    )


def digest(path: Path) -> str:
    sha = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            sha.update(chunk)
    return sha.hexdigest()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--target", help="Rust target triple (default: the host)")
    parser.add_argument(
        "--platform",
        required=True,
        choices=PLATFORMS,
        help="release key this build is for",
    )
    parser.add_argument("--skip-build", action="store_true", help="stage an existing build")
    parser.add_argument("--out", default="dist/binaries", help="output directory")
    args = parser.parse_args()

    target = args.target or host_target()
    if not args.skip_build:
        command = ["cargo", "build", "--release", "-p", "omni-cli", "--bins", "--target", target]
        print(f"cli-binaries: {' '.join(command)}", flush=True)
        subprocess.run(command, cwd=ROOT, check=True)

    directory = release_dir(target)
    suffix = ".exe" if "windows" in target else ""
    destination = ROOT / args.out
    destination.mkdir(parents=True, exist_ok=True)

    for binary in ("omni", "omniup"):
        source = directory / f"{binary}{suffix}"
        if not source.is_file():
            raise SystemExit(f"cli-binaries: {source} is missing; the build did not produce it")
        name = f"{binary}-{args.platform}{suffix}"
        staged = destination / name
        shutil.copy2(source, staged)
        checksum = digest(staged)
        (destination / f"{name}.sha256").write_text(
            f"{checksum}  {name}\n", encoding="utf-8"
        )
        size = staged.stat().st_size / (1 << 20)
        print(f"cli-binaries: {staged} ({size:.1f} MiB) sha256 {checksum}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
