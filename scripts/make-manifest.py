#!/usr/bin/env python3
"""Write the `omni-update.json` document a release check reads.

The engine compares versions; this script describes what is published. It
scans the staged executables, hashes the bytes it is about to advertise, and
writes one asset entry per platform:

    python scripts/make-manifest.py --tag v0.1.0
    python scripts/make-manifest.py --tag v0.1.0 --notes "faster calls"

The URL base is the GitHub Release download path for the tag, so the document
can be uploaded beside the binaries it names. `--base` overrides it for a
mirror, and `--allow-partial` writes a manifest that omits missing platforms
instead of refusing to describe an incomplete release.

Only the standard library is used, so it runs on a bare CI image.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

REPOSITORY = "https://github.com/fahim5536/omniengine"
PLATFORMS = ("win32-x64", "linux-x64", "linux-arm64", "darwin-x64", "darwin-arm64")


def version() -> str:
    text = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
    match = re.search(
        r"^\[workspace\.package\][\s\S]*?^version\s*=\s*\"([^\"]+)\"",
        text,
        re.MULTILINE,
    )
    if match is None:
        raise SystemExit("make-manifest: could not read the workspace version from Cargo.toml")
    return match.group(1)


def abi_version() -> int:
    text = (ROOT / "crates" / "omni-ffi" / "include" / "omni.h").read_text(encoding="utf-8")
    match = re.search(r"^#define\s+OMNI_ABI_VERSION\s+(\d+)", text, re.MULTILINE)
    if match is None:
        raise SystemExit("make-manifest: could not read OMNI_ABI_VERSION from omni.h")
    return int(match.group(1))


def digest(path: Path) -> str:
    sha = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            sha.update(chunk)
    return sha.hexdigest()


def binary_name(platform: str) -> str:
    return f"omni-{platform}.exe" if platform == "win32-x64" else f"omni-{platform}"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--tag", help="release tag the URLs point at, e.g. v0.1.0")
    parser.add_argument("--dir", default="dist/binaries", help="where the executables were staged")
    parser.add_argument("--out", default="dist/omni-update.json", help="document to write")
    parser.add_argument("--base", help="URL base instead of this repository's release download path")
    parser.add_argument("--notes", default="", help="one line the checkers can show")
    parser.add_argument(
        "--allow-partial",
        action="store_true",
        help="describe the platforms that are present instead of failing on a missing one",
    )
    args = parser.parse_args()

    found = version()
    abi = abi_version()
    if args.tag and args.tag.removeprefix("v") != found:
        raise SystemExit(
            f"make-manifest: tag {args.tag} names {args.tag.removeprefix('v')}, "
            f"but the workspace says {found}"
        )
    if not args.tag and not args.base:
        raise SystemExit("make-manifest: pass --tag (release URLs) or --base (a mirror)")

    base = args.base or f"{REPOSITORY}/releases/download/{args.tag}"
    if not base.endswith("/"):
        base += "/"

    directory = ROOT / args.dir
    if not directory.is_dir():
        raise SystemExit(f"make-manifest: {directory} does not exist; stage the binaries first")

    assets: dict[str, dict[str, str]] = {}
    missing: list[str] = []
    for platform in PLATFORMS:
        name = binary_name(platform)
        path = directory / name
        if not path.is_file():
            missing.append(name)
            continue
        checksum = digest(path)
        recorded = directory / f"{name}.sha256"
        if recorded.is_file():
            declared = recorded.read_text(encoding="utf-8").split()[0].strip().lower()
            if declared != checksum:
                raise SystemExit(
                    f"make-manifest: {recorded.name} says {declared}, but {name} hashes to "
                    f"{checksum}; the staged file and its checksum disagree"
                )
        assets[platform] = {"url": base + name, "sha256": checksum}

    if missing and not args.allow_partial:
        raise SystemExit(
            "make-manifest: no binary for " + ", ".join(missing) + "\n"
            "A release describes every platform; pass --allow-partial for a local draft."
        )
    if not assets:
        raise SystemExit("make-manifest: no platform binaries found; nothing to describe")

    document: dict[str, object] = {"version": found, "abi": abi}
    if args.notes:
        document["notes"] = args.notes
    document["assets"] = assets

    out = ROOT / args.out
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(document, indent=2) + "\n", encoding="utf-8")
    print(f"make-manifest: {out} describes {found} (ABI {abi}) for {', '.join(assets)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
