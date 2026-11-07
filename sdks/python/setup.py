"""Stage the native engine into a platform wheel.

The package metadata lives in ``pyproject.toml``. This file exists for one
reason: the engine is a Rust artifact, and a release wheel has to carry a copy
built from the tagged commit. setuptools can include a data file; it cannot
build a Rust workspace, so a small ``build_py`` subclass either copies an
artifact the caller built or runs cargo first.

``OMNI_NATIVE`` decides what happens:

===============  ==========================================================
unset or auto    stage ``target/release/<lib>`` if it exists; otherwise
                 leave the wheel pure (a checkout still finds ``target/``
                 at run time, and a published wheel never takes this path)
build            run ``cargo build --release -p omni-ffi``, then stage
skip             never stage (editable installs and the test suite)
/path/to/lib     stage that file, after checking its name and ABI
===============  ==========================================================

``OMNI_REQUIRE_NATIVE=1`` turns "nothing to stage" into an error. CI sets it so
a misconfigured release job cannot publish a wheel that will not load.
"""

from __future__ import annotations

import ctypes
import os
import re
import shutil
import subprocess
import sys
from pathlib import Path

from setuptools import Distribution, setup
from setuptools.command.build_py import build_py

ROOT = Path(__file__).resolve().parents[2]  # sdks/python -> repository root
PACKAGE = Path(__file__).resolve().parent / "omni"
NATIVE_SUBDIR = Path("_native")


def library_name() -> str:
    """The file name the running platform loads."""
    if sys.platform == "win32":
        return "omni.dll"
    if sys.platform == "darwin":
        return "libomni.dylib"
    return "libomni.so"


def expected_abi() -> int:
    """Read the SDK's ABI constant without importing (import loads a library)."""
    text = (PACKAGE / "_ffi.py").read_text(encoding="utf-8")
    match = re.search(r"^OMNI_ABI_VERSION\s*=\s*(\d+)", text, re.MULTILINE)
    if match is None:
        raise RuntimeError("OMNI_ABI_VERSION is missing from omni/_ffi.py")
    return int(match.group(1))


def release_artifact() -> Path | None:
    """A release-mode artifact already on disk, native or per-target."""
    name = library_name()
    direct = ROOT / "target" / "release" / name
    if direct.is_file():
        return direct
    for candidate in sorted((ROOT / "target").glob(f"*/release/{name}")):
        if candidate.is_file():
            return candidate
    return None


def cargo_build() -> Path:
    """Build the artifact the wheel will carry."""
    cargo = shutil.which("cargo")
    if cargo is None:
        raise RuntimeError(
            "OMNI_NATIVE=build asks for a bundled engine, but cargo is not on PATH.\n"
            "Install Rust (https://rustup.rs), or set OMNI_NATIVE to a prebuilt\n"
            f"{library_name()}."
        )
    print(f"omniengine: cargo build --release -p omni-ffi (in {ROOT})", flush=True)
    subprocess.run(
        [cargo, "build", "--release", "-p", "omni-ffi"],
        cwd=ROOT,
        check=True,
    )
    artifact = release_artifact()
    if artifact is None:
        raise RuntimeError(
            "cargo finished but no release artifact was found under target/release.\n"
            "Run `cargo build --release -p omni-ffi` yourself and set OMNI_NATIVE\n"
            "to the path it wrote."
        )
    return artifact


def verify(artifact: Path) -> Path:
    """Refuse a file this platform cannot load, or one speaking another ABI."""
    if artifact.name.lower() != library_name().lower():
        raise RuntimeError(
            f"OMNI_NATIVE names {artifact.name}, but this platform loads {library_name()}.\n"
            "A wheel must carry the library for the platform it is tagged for."
        )
    try:
        lib = ctypes.CDLL(str(artifact))
    except OSError as exc:
        raise RuntimeError(
            f"OMNI_NATIVE points at {artifact}, which this platform cannot load: {exc}\n"
            "Check that the file was built for this OS and architecture."
        ) from exc
    lib.omni_abi_version.restype = ctypes.c_uint32
    found = lib.omni_abi_version()
    wanted = expected_abi()
    if found != wanted:
        raise RuntimeError(
            f"OMNI_NATIVE carries ABI {found}, but this SDK speaks ABI {wanted}.\n"
            "Rebuild the engine from the same commit as the SDK."
        )
    print(f"omniengine: bundling {artifact.name} (ABI {found}) from {artifact}", flush=True)
    return artifact


def staging_artifact() -> Path | None:
    """The artifact this build will stage, or None for a pure wheel."""
    mode = os.environ.get("OMNI_NATIVE", "").strip() or "auto"
    if mode == "skip":
        return None
    if mode == "build":
        return verify(cargo_build())
    if mode == "auto":
        found = release_artifact()
        return verify(found) if found is not None else None

    candidate = Path(mode).expanduser()
    if candidate.is_dir():
        candidate = candidate / library_name()
    if not candidate.is_file():
        raise RuntimeError(f"OMNI_NATIVE={mode} is not a file or a directory containing one")
    return verify(candidate.resolve())


class BuildPy(build_py):
    """Copy the staged artifact beside the package, after the .py files land."""

    def run(self) -> None:
        artifact = staging_artifact()
        required = os.environ.get("OMNI_REQUIRE_NATIVE") == "1"
        if artifact is None:
            if required:
                raise RuntimeError(
                    "OMNI_REQUIRE_NATIVE=1 but no native artifact was staged.\n"
                    "Set OMNI_NATIVE=build, or point it at a built library."
                )
            print(
                "omniengine: no native artifact staged; the runtime will search\n"
                "target/ or OMNI_ENGINE_LIBRARY, as a checkout install expects.",
                flush=True,
            )
            super().run()
            return
        super().run()
        destination = Path(self.build_lib) / PACKAGE.name / NATIVE_SUBDIR
        destination.mkdir(parents=True, exist_ok=True)
        shutil.copy2(artifact, destination / artifact.name)


class PlatformWheel(Distribution):
    """Tag the wheel with the platform when it carries native code."""

    def has_ext_modules(self) -> bool:  # noqa: D102 - setuptools hook
        mode = os.environ.get("OMNI_NATIVE", "").strip() or "auto"
        if mode == "skip":
            return False
        if mode == "auto":
            return release_artifact() is not None
        return True


setup(cmdclass={"build_py": BuildPy}, distclass=PlatformWheel)
