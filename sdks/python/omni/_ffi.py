"""ctypes binding to OmniEngine's C ABI.

Private module. Import :mod:`omni`, not this.

Two rules carry over from the C header, and both are the reason a hand-rolled
Python FFI usually breaks in production:

1. **Every argument and return type is declared.** Without ``argtypes`` and
   ``restype``, ctypes assumes ``int`` and silently truncates a 64-bit engine
   handle to 32 bits -- which works on small heaps, then segfaults later.
2. **Nothing outlives a call as a pointer.** The engine frees its arena at the
   start of every evaluation, so results are converted into Python objects
   before they leave this module.
"""

from __future__ import annotations

import atexit
import ctypes
import os
import platform
import sys
from ctypes import (
    POINTER,
    Structure,
    byref,
    c_char_p,
    c_double,
    c_size_t,
    c_uint32,
    c_void_p,
)

__all__ = [
    "STATUS",
    "TAGS",
    "MAX_SOURCE_LEN",
    "OmniValue",
    "RawEngine",
    "OmniError",
    "OmniLibraryError",
    "load_library",
]

# ---------------------------------------------------------------------------
# Constants, kept in sync with crates/omni-ffi/src/lib.rs and include/omni.h.
# ---------------------------------------------------------------------------

OMNI_OK = 0
OMNI_ERR_COMPILE = 1
OMNI_ERR_RUNTIME = 2
OMNI_ERR_PANIC = 3
OMNI_ERR_ARG = 4

STATUS = {
    OMNI_OK: "ok",
    OMNI_ERR_COMPILE: "compile",
    OMNI_ERR_RUNTIME: "runtime",
    OMNI_ERR_PANIC: "panic",
    OMNI_ERR_ARG: "argument",
}

OMNI_NUMBER = 0
OMNI_NIL = 1
OMNI_BOOL = 2
OMNI_UNDEF = 3

TAGS = {
    OMNI_NUMBER: "number",
    OMNI_NIL: "nil",
    OMNI_BOOL: "bool",
    OMNI_UNDEF: "undef",
}

#: The ABI rejects any ``len`` above this, so a huge value is an argument error
#: rather than an out-of-bounds read.
MAX_SOURCE_LEN = 1 << 28


class OmniValue(Structure):
    """Mirror of ``OmniValue`` in omni.h: ``{ uint32_t tag; double number; }``."""

    _fields_ = [("tag", c_uint32), ("number", c_double)]


# Fail loudly at import time if the struct ever drifts from the Rust layout,
# instead of returning garbage numbers at run time.
if ctypes.sizeof(OmniValue) != 16:  # pragma: no cover - layout regression
    raise RuntimeError(
        f"OmniValue is {ctypes.sizeof(OmniValue)} bytes, expected 16. "
        "The Python SDK and crates/omni-ffi disagree about the ABI."
    )
if OmniValue.number.offset != 8:  # pragma: no cover - layout regression
    raise RuntimeError(
        f"OmniValue.number is at offset {OmniValue.number.offset}, expected 8. "
        "The Python SDK and crates/omni-ffi disagree about the ABI."
    )


class OmniError(RuntimeError):
    """A program OmniEngine rejected, or a call a host made incorrectly.

    Attributes:
        status: the raw ``u32`` the ABI returned.
        kind: ``"compile"``, ``"runtime"``, ``"panic"``, or ``"argument"``.
        message: the engine's own text, already copied out of the engine.
    """

    def __init__(self, status: int, message: str) -> None:
        super().__init__(f"{STATUS.get(status, status)}: {message}" if message else STATUS.get(status, str(status)))
        self.status = status
        self.kind = STATUS.get(status, f"status-{status}")
        self.message = message


class OmniLibraryError(OSError):
    """The shared library could not be found. Raised with the fix in it."""


# ---------------------------------------------------------------------------
# Library discovery
# ---------------------------------------------------------------------------


def library_file_name() -> str:
    """The platform-specific file name a host loads."""
    if sys.platform == "win32":
        return "omni.dll"
    if sys.platform == "darwin":
        return "libomni.dylib"
    return "libomni.so"


def _upward(base: str, name: str, levels: int = 6) -> list[str]:
    """`base` and each of its parents, as a place a cargo target/ could live."""
    found: list[str] = []
    current = os.path.abspath(base)
    for _ in range(levels + 1):
        # Check `current` before moving up, so the directory passed in counts --
        # that is the common case of running from the repository root.
        for profile in ("debug", "release"):
            found.append(os.path.join(current, "target", profile, name))
        parent = os.path.dirname(current)
        if parent == current:  # filesystem root
            break
        current = parent
    return found


def _candidate_paths() -> list[str]:
    """Every place the artifact could be, most explicit first.

    ``OMNI_ENGINE_LIBRARY`` names a file, so it wins outright.
    ``OMNI_ENGINE_HOME`` names a project root, and we look in the directories a
    build could reasonably have written to.
    Then, so an installed wheel works with no configuration at all: the package
    directory itself (a wheel that bundles the native artifact), the tree the
    package sits in, and the tree the current command was run from.
    """
    name = library_file_name()
    found: list[str] = []

    explicit = os.environ.get("OMNI_ENGINE_LIBRARY")
    if explicit:
        found.append(explicit)

    home = os.environ.get("OMNI_ENGINE_HOME")
    if home:
        for sub in ("", "target/debug", "target/release", "lib", "bin"):
            parts = [home] if sub == "" else [home, *sub.split("/")]
            found.append(os.path.join(*parts, name))

    package_dir = os.path.dirname(os.path.abspath(__file__))
    found.append(os.path.join(package_dir, name))  # bundled inside the wheel
    found.extend(_upward(package_dir, name))  # a checkout or an editable install
    found.extend(_upward(os.getcwd(), name))  # run from anywhere inside the repo

    # Order matters, but duplicates from the two walks do not.
    seen: set[str] = set()
    unique: list[str] = []
    for path in found:
        key = os.path.normcase(os.path.abspath(path))
        if key not in seen:
            seen.add(key)
            unique.append(path)
    return unique


def load_library() -> ctypes.CDLL:
    """Load OmniEngine and declare the whole ABI on it.

    Raises:
        OmniLibraryError: if no artifact was found. The message carries the
            build command, because every failure of this function so far has
            been "you have not built the library yet".
    """
    tried = _candidate_paths()
    for path in tried:
        if path and os.path.isfile(path):
            return _bind(_cdll(path))

    # Fall back to whatever the loader can resolve by bare name (a system-wide
    # install, or a directory on PATH).
    name = library_file_name()
    try:
        return _bind(ctypes.CDLL(name))
    except OSError as exc:
        raise OmniLibraryError(
            f"could not find {name}.\n"
            "Build it first:\n"
            "    cargo build -p omni-ffi\n"
            "The SDK then finds it on its own in any target/debug or\n"
            "target/release directory above this package or above the current\n"
            "working directory.\n"
            "To use a library somewhere else, set OMNI_ENGINE_LIBRARY to its\n"
            "full path, or OMNI_ENGINE_HOME to the project root.\n"
            f"Locations tried:\n{chr(10).join('  ' + p for p in tried)}\n"
            f"Loader said: {exc}"
        ) from exc


def _cdll(path: str) -> ctypes.CDLL:
    # On Windows, make the artifact's own directory searchable so a future
    # dependency sitting next to omni.dll resolves the same way it does now.
    if platform.system() == "Windows" and hasattr(os, "add_dll_directory"):
        directory = os.path.dirname(os.path.abspath(path))
        if os.path.isdir(directory):
            os.add_dll_directory(directory)
    return ctypes.CDLL(path)


def _bind(lib: ctypes.CDLL) -> ctypes.CDLL:
    """Attach signatures to every export.

    This is the part that is easy to skip and impossible to debug once skipped:
    a missing ``restype`` on ``omni_engine_new`` returns a truncated 32-bit
    handle that looks valid until the first call that dereferences it.
    """
    lib.omni_version.argtypes = []
    lib.omni_version.restype = c_char_p

    lib.omni_engine_new.argtypes = []
    lib.omni_engine_new.restype = c_void_p

    lib.omni_engine_free.argtypes = [c_void_p]
    lib.omni_engine_free.restype = None

    lib.omni_exec.argtypes = [c_void_p, c_char_p, c_size_t, POINTER(OmniValue)]
    lib.omni_exec.restype = c_uint32

    lib.omni_exec_number.argtypes = [c_void_p, c_char_p, c_size_t, POINTER(c_double)]
    lib.omni_exec_number.restype = c_uint32

    lib.omni_last_error.argtypes = [c_void_p, c_char_p, c_size_t]
    lib.omni_last_error.restype = c_size_t

    lib.omni_debug_panic.argtypes = [c_void_p]
    lib.omni_debug_panic.restype = c_uint32
    return lib


# ---------------------------------------------------------------------------
# Handle wrapper
# ---------------------------------------------------------------------------


class RawEngine:
    """One engine handle, one thread.

    Thin on purpose: :class:`omni.Engine` is the API people should use. This
    exists so the tests can exercise the ABI directly, including the argument
    errors the friendly wrapper prevents.

    The handle is freed on :meth:`close`, and on garbage collection only if the
    interpreter is not shutting down -- calling into a unloaded library during
    interpreter teardown is how these bindings segfault on exit.
    """

    __slots__ = ("_lib", "_handle")

    def __init__(self, lib: ctypes.CDLL | None = None) -> None:
        self._lib = lib if lib is not None else load_library()
        handle = self._lib.omni_engine_new()
        if not handle:
            raise OmniLibraryError("omni_engine_new returned a null handle")
        self._handle = handle

    @property
    def lib(self) -> ctypes.CDLL:
        return self._lib

    def version(self) -> str:
        raw = self._lib.omni_version()
        return raw.decode("utf-8", "replace") if raw else ""

    def exec(self, source: str | bytes) -> tuple[int, OmniValue]:
        """Evaluate and return ``(status, value)``. Never raises on error."""
        data = self._encode(source)
        out = OmniValue()
        status = self._lib.omni_exec(self._handle, data, len(data), byref(out))
        return status, out

    def exec_number(self, source: str | bytes) -> tuple[int, float]:
        data = self._encode(source)
        out = c_double(0.0)
        status = self._lib.omni_exec_number(self._handle, data, len(data), byref(out))
        return status, out.value

    def last_error(self, cap: int = 4096) -> str:
        buf = ctypes.create_string_buffer(cap)
        written = self._lib.omni_last_error(self._handle, buf, cap)
        return buf.raw[:written].decode("utf-8", "replace")

    def debug_panic(self) -> int:
        return self._lib.omni_debug_panic(self._handle)

    def close(self) -> None:
        if self._handle:
            self._lib.omni_engine_free(self._handle)
            self._handle = None

    @staticmethod
    def _encode(source: str | bytes) -> bytes:
        """Encode to UTF-8 and bound the length the way the ABI does."""
        data = source.encode("utf-8") if isinstance(source, str) else bytes(source)
        if len(data) > MAX_SOURCE_LEN:
            raise OmniError(
                OMNI_ERR_ARG,
                f"source is {len(data)} bytes; the ABI accepts at most {MAX_SOURCE_LEN}",
            )
        return data

    def __enter__(self) -> "RawEngine":
        return self

    def __exit__(self, *exc: object) -> bool:
        self.close()
        return False

    def __del__(self) -> None:  # pragma: no cover - best effort
        # During interpreter shutdown the library may already be gone.
        if getattr(self, "_handle", None) and not _SHUTTING_DOWN:
            self.close()


# True once the interpreter has begun teardown; set from an atexit hook that
# registered before anything else could be collected.
_SHUTTING_DOWN = False


def _note_shutdown() -> None:
    global _SHUTTING_DOWN

    _SHUTTING_DOWN = True


atexit.register(_note_shutdown)
