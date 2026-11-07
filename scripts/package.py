#!/usr/bin/env python3
"""Build a clean, versioned native distribution archive for one target.

An archive contains the shared library, its import library when the platform
produces one, the public header, the C embedding example, README and LICENSE.
Nothing else: no target/ tree, no editor files, no engine sources.

    python scripts/package.py                          # host target, release build
    python scripts/package.py --skip-build             # archive what is on disk
    python scripts/package.py --target aarch64-apple-darwin --format tar.gz

The release workflow calls this once per platform. It uses only the standard
library, so it runs on a bare CI image with nothing installed but Rust and
Python.
"""

from __future__ import annotations

import argparse
import hashlib
import os
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def library_name() -> str:
    if os.name == "nt" or sys.platform == "win32":
        return "omni.dll"
    if sys.platform == "darwin":
        return "libomni.dylib"
    return "libomni.so"


def version() -> str:
    text = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
    match = re.search(
        r"^\[workspace\.package\][\s\S]*?^version\s*=\s*\"([^\"]+)\"",
        text,
        re.MULTILINE,
    )
    if match is None:
        raise SystemExit("package: could not read the workspace version from Cargo.toml")
    return match.group(1)


def host_target() -> str:
    result = subprocess.run(
        ["rustc", "-vV"], check=True, capture_output=True, text=True
    )
    match = re.search(r"^host:\s*(\S+)$", result.stdout, re.MULTILINE)
    if match is None:
        raise SystemExit("package: rustc -vV did not report a host target")
    return match.group(1)


def build(target: str) -> None:
    command = ["cargo", "build", "--release", "-p", "omni-ffi", "--target", target]
    print(f"package: {' '.join(command)}", flush=True)
    subprocess.run(command, cwd=ROOT, check=True)


def release_dir(target: str) -> Path:
    # A --target build writes under target/<triple>/release; a plain host build
    # writes under target/release. Accept either, per-target first.
    per_target = ROOT / "target" / target / "release"
    if per_target.is_dir():
        return per_target
    plain = ROOT / "target" / "release"
    if plain.is_dir() and target == host_target():
        return plain
    raise SystemExit(
        f"package: no release directory for {target}.\n"
        "Build it first: cargo build --release -p omni-ffi --target " + target
    )


def shared_libraries(directory: Path) -> list[Path]:
    """The files a host can load or link against, in a stable order."""
    candidates = [library_name(), "libomni.dll.a", "omni.dll.lib"]
    found = [directory / name for name in candidates if (directory / name).is_file()]
    if not any(path.suffix in (".dll", ".so", ".dylib") for path in found):
        raise SystemExit(f"package: {directory} has no shared library to ship")
    return found


def stage(root: Path, libs: list[Path]) -> Path:
    name = f"omniengine-{version()}-{root.name}"
    staged = root / name
    (staged / "lib").mkdir(parents=True)
    (staged / "include").mkdir()
    (staged / "examples" / "c").mkdir(parents=True)

    for lib in libs:
        shutil.copy2(lib, staged / "lib" / lib.name)
    shutil.copy2(ROOT / "crates" / "omni-ffi" / "include" / "omni.h", staged / "include")
    shutil.copy2(ROOT / "examples" / "c" / "embed.c", staged / "examples" / "c")
    shutil.copy2(ROOT / "README.md", staged)
    shutil.copy2(ROOT / "LICENSE", staged)
    return staged


def archive(staged: Path, destination: Path, fmt: str) -> Path:
    destination.mkdir(parents=True, exist_ok=True)
    if fmt == "zip":
        out = destination / f"{staged.name}.zip"
        with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED) as zf:
            for path in sorted(staged.rglob("*")):
                if path.is_file():
                    arcname = str(path.relative_to(staged.parent)).replace(os.sep, "/")
                    zf.write(path, arcname)
    else:
        out = destination / f"{staged.name}.tar.gz"
        with tarfile.open(out, "w:gz") as tf:
            tf.add(staged, arcname=staged.name)
    return out


def digest(path: Path) -> str:
    sha = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            sha.update(chunk)
    return sha.hexdigest()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--target", help="Rust target triple (default: the host)")
    parser.add_argument("--skip-build", action="store_true", help="archive an existing build")
    parser.add_argument(
        "--format",
        choices=("auto", "zip", "tar.gz"),
        default="auto",
        help="archive format (auto: zip on Windows, tar.gz elsewhere)",
    )
    parser.add_argument("--out", default="dist", help="output directory (default: dist)")
    args = parser.parse_args()

    target = args.target or host_target()
    if not args.skip_build:
        build(target)
    libs = shared_libraries(release_dir(target))

    fmt = args.format
    if fmt == "auto":
        fmt = "zip" if os.name == "nt" else "tar.gz"

    with tempfile.TemporaryDirectory(prefix="omniengine-package-") as temp:
        staged = stage(Path(temp) / target, libs)
        out = archive(staged, ROOT / args.out, fmt)

    checksum = digest(out)
    (out.with_name(out.name + ".sha256")).write_text(
        f"{checksum}  {out.name}\n", encoding="utf-8"
    )
    size = out.stat().st_size / (1 << 20)
    print(f"package: {out} ({size:.1f} MiB)")
    print(f"package: sha256 {checksum}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
