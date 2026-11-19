#!/usr/bin/env python3
"""Verify built distribution artifacts before they are handed to strangers.

Unpacks every artifact it is pointed at -- native archives (``*.zip`` /
``*.tar.gz``), Python wheels (``*.whl``), and the npm tarball (``*.tgz``) --
and checks, without loading any of the binaries:

1.  the shared library has the container magic its platform promises
    (PE ``MZ`` / ELF ``\\x7fELF`` / Mach-O ``feedface`` variants);
2.  the library's dynamic symbols include the three permission exports
    (``omni_engine_allow_fs_read`` / ``omni_engine_allow_fs_write`` /
    ``omni_engine_set_module_root``) -- the binary-level proof that the
    artifact speaks ABI 5, since an ABI 4 build predates those symbols and
    cannot pass this check no matter what its header says;
3.  every artifact carries the VGPIL 1.0 license text;
4.  the ABI constants in ``omni.h``, the Python SDK, the Node SDK, and the
    CLI updater agree with each other and with ``--abi`` (which defaults to
    the ``omni.h`` value), and an ``omni-update.json`` manifest in the scanned
    directory agrees too;
5.  the npm tarball carries one valid engine under ``native/<platform>/`` for
    every platform directory present, and ``--require-all-platforms`` demands
    all five -- the release-workflow gate, where a partial tarball is a
    release bug rather than a checkout convenience;
6.  a ``*.sha256`` sidecar, when present, matches its archive's bytes.

Reads files only. It never builds, loads, downloads, or publishes anything;
loading an unpacked artifact is the integration half of ``verify-all.sh`` row
40, and ``scripts/prepack.js`` remains the pre-publish guard.

    python scripts/package-verify.py                     # dist/ + sdks/nodejs/*.tgz
    python scripts/package-verify.py dist/*.whl ...      # explicit artifact paths
    python scripts/package-verify.py --require-all-platforms   # the CI gate
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import sys
import tarfile
import tempfile
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

LICENSE_MARK = "Vextalor Global Public & Integration License"
PERMISSION_EXPORTS = (
    b"omni_engine_allow_fs_read",
    b"omni_engine_allow_fs_write",
    b"omni_engine_set_module_root",
)

# platform key -> (library name, container magic family)
PLATFORMS = {
    "win32-x64": ("omni.dll", "pe"),
    "linux-x64": ("libomni.so", "elf"),
    "linux-arm64": ("libomni.so", "elf"),
    "darwin-x64": ("libomni.dylib", "macho"),
    "darwin-arm64": ("libomni.dylib", "macho"),
}

MAGICS = {
    "pe": lambda b: b[:2] == b"MZ",
    "elf": lambda b: b[:4] == b"\x7fELF",
    # little- and big-endian, 32/64-bit, thin and fat binaries
    "macho": lambda b: b[:4]
    in (b"\xfe\xed\xfa\xce", b"\xfe\xed\xfa\xcf", b"\xce\xfa\xed\xfe", b"\xcf\xfa\xed\xfe")
    or b[:4] in (b"\xca\xfe\xba\xbe", b"\xbe\xba\xfe\xca"),
}


class Report:
    def __init__(self) -> None:
        self.failures: list[str] = []

    def fail(self, message: str) -> None:
        self.failures.append(message)
        print(f"  FAIL  {message}")


def read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def capture(pattern: str, path: Path, what: str) -> str:
    match = re.search(pattern, read(path), re.MULTILINE)
    if match is None:
        raise SystemExit(f"package-verify: could not read {what} from {path}")
    return match.group(1)


def expected_abi() -> str:
    return capture(
        r"^#define\s+OMNI_ABI_VERSION\s+(\d+)",
        ROOT / "crates" / "omni-ffi" / "include" / "omni.h",
        "OMNI_ABI_VERSION",
    )


def surface_abis() -> dict[str, str]:
    return {
        "omni.h": capture(
            r"^#define\s+OMNI_ABI_VERSION\s+(\d+)",
            ROOT / "crates" / "omni-ffi" / "include" / "omni.h",
            "OMNI_ABI_VERSION",
        ),
        "Python SDK": capture(
            r"^OMNI_ABI_VERSION\s*=\s*(\d+)",
            ROOT / "sdks" / "python" / "omni" / "_ffi.py",
            "OMNI_ABI_VERSION",
        ),
        "Node SDK": capture(
            r"^const ABI_VERSION\s*=\s*(\d+);",
            ROOT / "sdks" / "nodejs" / "lib" / "bindings.js",
            "ABI_VERSION",
        ),
        "CLI updater": capture(
            r"^pub const ABI_VERSION: u32 = (\d+);",
            ROOT / "crates" / "omni-cli" / "src" / "updater" / "mod.rs",
            "ABI_VERSION",
        ),
    }


class Archive:
    """A read-only view over one artifact, whatever container it came in."""

    def __init__(self, path: Path) -> None:
        self.path = path
        self.members: dict[str, bytes] = {}
        if path.suffix == ".whl" or path.suffix == ".zip":
            with zipfile.ZipFile(path) as zf:
                for info in zf.infolist():
                    if info.file_size > 0 and not info.filename.endswith("/"):
                        self.members[info.filename] = zf.read(info.filename)
        elif path.name.endswith(".tar.gz") or path.name.endswith(".tgz"):
            with tarfile.open(path, "r:gz") as tf:
                for member in tf.getmembers():
                    if member.isfile():
                        self.members[member.name] = tf.extractfile(member).read()
        else:
            raise SystemExit(f"package-verify: unrecognized artifact type: {path}")

    def find(self, *needles: str) -> list[str]:
        """Member names whose basename is one of the given names, sorted."""
        out = []
        for name in self.members:
            if name.rsplit("/", 1)[-1] in needles:
                out.append(name)
        return sorted(out)

    def has_license(self) -> bool:
        return any(LICENSE_MARK.encode() in blob for blob in self.members.values())


def check_library(report: Report, label: str, blob: bytes, family: str | None) -> bool:
    """Container magic plus the ABI 5 permission-export symbol proof."""
    ok = True
    if family is not None and not MAGICS[family](blob):
        report.fail(f"{label}: not a {family.upper()} image")
        ok = False
    for symbol in PERMISSION_EXPORTS:
        if symbol not in blob:
            report.fail(
                f"{label}: missing export {symbol.decode()} -- this binary predates "
                "the ABI 5 permission surface"
            )
            ok = False
    return ok


def platform_of(name: str) -> str | None:
    lowered = name.lower()
    if "windows" in lowered or "win_amd64" in lowered or "win32" in lowered:
        return "win32-x64"
    if "apple" in lowered or "macosx" in lowered or "darwin" in lowered:
        return "darwin-arm64" if "arm64" in lowered or "aarch64" in lowered else "darwin-x64"
    if "linux" in lowered or "manylinux" in lowered:
        return "linux-arm64" if "aarch64" in lowered or "arm64" in lowered else "linux-x64"
    return None


def verify_native_archive(report: Report, path: Path, abi: str) -> None:
    archive = Archive(path)
    stem = path.name
    for needle in ("omni.h", "LICENSE", "embed.c"):
        if not archive.find(needle):
            report.fail(f"{stem}: archive has no {needle}")
    header = archive.find("omni.h")
    if header:
        text = archive.members[header[0]].decode("utf-8", "replace")
        match = re.search(r"^#define\s+OMNI_ABI_VERSION\s+(\d+)", text, re.MULTILINE)
        if match is None or match.group(1) != abi:
            report.fail(
                f"{stem}: bundled omni.h declares ABI "
                f"{match.group(1) if match else '?'}, expected {abi}"
            )
    libs = archive.find("omni.dll", "libomni.so", "libomni.dylib")
    if not libs:
        report.fail(f"{stem}: archive has no shared library")
        return
    if not archive.has_license():
        report.fail(f"{stem}: archive has no VGPIL license text")
    family = PLATFORMS.get(platform_of(stem) or "", ("", None))[1]
    check_library(report, f"{stem}:{Path(libs[0]).name}", archive.members[libs[0]], family)


def verify_wheel(report: Report, path: Path, abi: str) -> None:
    archive = Archive(path)
    stem = path.name
    metadata = archive.find("METADATA")
    if not metadata:
        report.fail(f"{stem}: wheel has no METADATA")
    elif not re.search(rb"^Name:\s*omniengine", archive.members[metadata[0]], re.MULTILINE):
        report.fail(f"{stem}: METADATA is not for the omniengine package")
    libs = archive.find("omni.dll", "libomni.so", "libomni.dylib")
    if not libs:
        report.fail(f"{stem}: wheel bundles no engine under omni/_native")
        return
    if not any("_native" in name for name in libs):
        report.fail(f"{stem}: bundled engine is not under omni/_native")
    if not archive.has_license():
        report.fail(f"{stem}: wheel has no VGPIL license text")
    family = PLATFORMS.get(platform_of(stem) or "", ("", None))[1]
    check_library(report, f"{stem}:{Path(libs[0]).name}", archive.members[libs[0]], family)


def verify_npm_tarball(report: Report, path: Path, abi: str, require_all: bool) -> None:
    archive = Archive(path)
    stem = path.name
    manifest_names = archive.find("package.json")
    if not manifest_names:
        report.fail(f"{stem}: tarball has no package.json")
    else:
        pkg = json.loads(archive.members[manifest_names[0]])
        if pkg.get("name") != "omniengine":
            report.fail(f"{stem}: package.json names {pkg.get('name')!r}, expected 'omniengine'")
    if not archive.has_license():
        report.fail(f"{stem}: tarball has no VGPIL license text")

    present = []
    for platform, (lib_name, family) in PLATFORMS.items():
        members = sorted(
            n for n in archive.members if n.endswith(f"native/{platform}/{lib_name}")
        )
        if members:
            present.append(platform)
            check_library(
                report,
                f"{stem}:native/{platform}/{lib_name}",
                archive.members[members[0]],
                family,
            )
    if not present:
        report.fail(f"{stem}: tarball bundles no platform engines at all")
    missing = [p for p in PLATFORMS if p not in present]
    if require_all and missing:
        report.fail(
            f"{stem}: missing platform bundles: {', '.join(missing)} "
            "(all five are required for a release tarball)"
        )


def verify_sidecar(report: Report, sidecar: Path) -> None:
    target = sidecar.with_name(sidecar.name.removesuffix(".sha256"))
    if not target.is_file():
        report.fail(f"{sidecar.name}: no archive next to the sidecar")
        return
    recorded = read(sidecar).split()[0].lower()
    digest = hashlib.sha256(target.read_bytes()).hexdigest()
    if recorded != digest:
        report.fail(f"{sidecar.name}: checksum {recorded[:16]}... does not match {target.name}")


def default_artifacts(dist: Path) -> list[Path]:
    found: list[Path] = []
    if dist.is_dir():
        found += sorted(dist.glob("omniengine-*.zip"))
        found += sorted(dist.glob("omniengine-*.tar.gz"))
        found += sorted(dist.glob("*.whl"))
        found += sorted(dist.glob("omni-update.json"))
    found += sorted((ROOT / "sdks" / "nodejs").glob("omniengine-*.tgz"))
    return found


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "artifacts",
        nargs="*",
        help="artifact paths to verify (default: dist/ + sdks/nodejs/*.tgz)",
    )
    parser.add_argument("--dist", default="dist", help="artifact directory (default: dist)")
    parser.add_argument(
        "--abi",
        default=None,
        help="expected ABI level (default: read from omni.h)",
    )
    parser.add_argument(
        "--require-all-platforms",
        action="store_true",
        help="the npm tarball must carry all five platform engines (CI gate)",
    )
    args = parser.parse_args()

    abi = args.abi or expected_abi()
    report = Report()

    surfaces = surface_abis()
    if len(set(surfaces.values())) != 1:
        report.fail("ABI mismatch across surfaces: " + ", ".join(f"{k} {v}" for k, v in surfaces.items()))
    elif surfaces["omni.h"] != abi:
        report.fail(f"omni.h declares ABI {surfaces['omni.h']} but --abi says {abi}")

    paths = [Path(p) for p in args.artifacts] or default_artifacts(Path(args.dist))
    if not paths:
        report.fail(f"no artifacts found to verify (looked in {args.dist}/ and sdks/nodejs/)")

    manifest_seen: str | None = None
    for path in paths:
        if not path.is_file():
            report.fail(f"{path}: no such file")
            continue
        if path.name == "omni-update.json":
            manifest = json.loads(read(path))
            manifest_seen = str(manifest.get("abi"))
            if manifest_seen != abi:
                report.fail(f"{path.name}: manifest speaks ABI {manifest_seen}, expected {abi}")
            continue
        if path.name.endswith(".sha256"):
            verify_sidecar(report, path)
            continue
        print(f"verify {path.name}")
        try:
            if path.suffix == ".whl":
                verify_wheel(report, path, abi)
            elif path.name.endswith(".tgz"):
                verify_npm_tarball(report, path, abi, args.require_all_platforms)
            else:
                verify_native_archive(report, path, abi)
        except (zipfile.BadZipFile, tarfile.TarError) as error:
            report.fail(f"{path.name}: unreadable archive: {error}")

    if report.failures:
        print(f"package-verify: {len(report.failures)} problem(s) across {len(paths)} artifact(s)")
        return 1
    platforms = "all five platforms" if args.require_all_platforms else "present platforms"
    print(f"package-verify: ABI {abi} verified across {len(paths)} artifact(s); npm bundle covers {platforms}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
