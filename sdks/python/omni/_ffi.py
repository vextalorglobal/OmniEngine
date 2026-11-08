"""ctypes binding to OmniEngine's C ABI.

Private module. Import :mod:`omni`, not this.

Three rules carry over from the C header, and each is the reason a hand-rolled
Python FFI usually breaks in production:

1. **Every argument and return type is declared.** Without ``argtypes`` and
   ``restype``, ctypes assumes ``int`` and silently truncates a 64-bit engine
   handle to 32 bits -- which works on small heaps, then segfaults later.
2. **Nothing outlives a call as a pointer.** A string result's ``payload.ptr``
   addresses the engine's egress block, which is *rewound* at the start of the
   next evaluation rather than freed. So a stale address still points at
   mapped, engine-owned memory and reads that execution's bytes: a wrong read,
   never a crash. This module refuses to let that happen by handing a Python
   object an address at all -- :meth:`RawEngine.read_string` copies the bytes
   out through ``omni_str_copy`` before the value leaves this file.
3. **The ABI level is checked at load.** The value layout changed between
   version 1 and 2; ``load_library`` compares ``omni_abi_version()`` with what
   this module declares and refuses to return a library that disagrees, so a
   stale ``omni.dll`` is one clear error at import instead of a wrong number
   ten thousand calls later.
4. **A callback's memory is the engine's, and only while the callback runs.**
   Version 3 lets Python own a function a script calls. Inside that function,
   an argument string points into the engine's arena and is copied with
   :func:`ctypes.string_at` -- :func:`omni_str_copy` cannot be used, because it
   is a call into the engine and the engine is already inside the call that
   invoked us. Conversely, a string we hand *back* must outlive the return: the
   engine reads ``out`` after our trampoline has already given control to C, so
   each binding keeps its last output buffer until the next call replaces it.
"""

from __future__ import annotations

import atexit
import ctypes
import os
import platform
import sys
from collections.abc import Callable, Sequence
from typing import Any
from ctypes import (
    POINTER,
    Structure,
    Union,
    byref,
    c_char_p,
    c_double,
    c_int32,
    c_size_t,
    c_uint32,
    c_uint64,
    c_void_p,
    create_string_buffer,
    string_at,
)
from dataclasses import dataclass

__all__ = [
    "STATUS",
    "TAGS",
    "MAX_SOURCE_LEN",
    "MAX_STRING_LEN",
    "MAX_IDENTIFIER_LEN",
    "MAX_ARGS",
    "MAX_CALL_DEPTH",
    "MAX_BATCH",
    "OMNI_ABI_VERSION",
    "OMNI_HOST_OK",
    "OMNI_HOST_ERR",
    "OMNI_UPDATE_UP_TO_DATE",
    "OMNI_UPDATE_AVAILABLE",
    "OMNI_UPDATE_ABI_MISMATCH",
    "OMNI_UPDATE_INVALID",
    "OMNI_UPDATE_BAD_ARG",
    "UPDATE_STATUS",
    "MAX_MANIFEST_LEN",
    "HOST_FN",
    "OmniPayload",
    "OmniValue",
    "OmniStats",
    "Stats",
    "RawEngine",
    "HostFunction",
    "OmniError",
    "OmniLibraryError",
    "load_library",
    "check_update",
]

# ---------------------------------------------------------------------------
# Constants, kept in sync with crates/omni-ffi/src/lib.rs and include/omni.h.
# ---------------------------------------------------------------------------

OMNI_OK = 0
OMNI_ERR_COMPILE = 1
OMNI_ERR_RUNTIME = 2
OMNI_ERR_PANIC = 3
OMNI_ERR_ARG = 4
OMNI_ERR_HOST = 5
OMNI_ERR_REENTRANT = 6

STATUS = {
    OMNI_OK: "ok",
    OMNI_ERR_COMPILE: "compile",
    OMNI_ERR_RUNTIME: "runtime",
    OMNI_ERR_PANIC: "panic",
    OMNI_ERR_ARG: "argument",
    OMNI_ERR_HOST: "host",
    OMNI_ERR_REENTRANT: "reentrant",
}

OMNI_NUMBER = 0
OMNI_NIL = 1
OMNI_BOOL = 2
OMNI_UNDEF = 3
OMNI_STRING = 4

TAGS = {
    OMNI_NUMBER: "number",
    OMNI_NIL: "nil",
    OMNI_BOOL: "bool",
    OMNI_UNDEF: "undef",
    OMNI_STRING: "string",
}

#: A host function's own return values: 0 for success, 1 for "out is a message".
OMNI_HOST_OK = 0
OMNI_HOST_ERR = 1

#: What ``omni_check_update`` answers. These are an additive export inside ABI 4:
#: the structs and every other signature are unchanged, so a library built
#: before this export still loads -- only the update check is unavailable.
OMNI_UPDATE_UP_TO_DATE = 0
OMNI_UPDATE_AVAILABLE = 1
OMNI_UPDATE_ABI_MISMATCH = 2
OMNI_UPDATE_INVALID = 3
OMNI_UPDATE_BAD_ARG = -1

UPDATE_STATUS = {
    OMNI_UPDATE_UP_TO_DATE: "up-to-date",
    OMNI_UPDATE_AVAILABLE: "available",
    OMNI_UPDATE_ABI_MISMATCH: "abi-mismatch",
    OMNI_UPDATE_INVALID: "invalid",
    OMNI_UPDATE_BAD_ARG: "bad-argument",
}

#: Largest manifest ``omni_check_update`` will read. Mirrors
#: ``omni_core::update::MAX_MANIFEST_LEN``; the engine refuses a longer document
#: with ``OMNI_UPDATE_BAD_ARG`` rather than parsing it.
MAX_MANIFEST_LEN = 1 << 20

#: Wire format of the value struct this module reads. Checked against the
#: library at load; see the module docstring's rule 3.
OMNI_ABI_VERSION = 4

#: The ABI rejects any source ``len`` above this, so a huge value is an argument
#: error rather than an out-of-bounds read.
MAX_SOURCE_LEN = 1 << 28

#: Longest string the engine will build, and the most ``omni_str_copy`` can ever
#: write. Mirrors ``OMNI_MAX_STRING_LEN``; these are ceilings a host sizes
#: buffers with, so the cost of a mismatch is a resized buffer, not a fault.
MAX_STRING_LEN = 16 * 1024 * 1024

#: Longest name a host may bind, in *bytes* of UTF-8. Mirrors
#: ``OMNI_MAX_IDENTIFIER_LEN``.
MAX_IDENTIFIER_LEN = 32

#: Most arguments a call may pass, and the highest arity a registration may
#: declare. Mirrors ``OMNI_MAX_ARGS``.
MAX_ARGS = 16

#: Deepest a script's calls may nest inside one execution. Mirrors
#: ``OMNI_MAX_CALL_DEPTH``; the engine refuses to go past it, so a recursive
#: host function that never terminates costs a runtime error, not a stack
#: overflow that would take the Python process down with it.
MAX_CALL_DEPTH = 64

#: Most programs one ``omni_exec_many`` crossing may carry. Mirrors
#: ``OMNI_MAX_BATCH``: a boundary bound on how many pointers the engine will read
#: from a host array, not a limit of the language.
MAX_BATCH = 4096


#: What a host function receives and returns: the arguments the script passed,
#: lowered to ``float`` / ``bool`` / ``str`` / ``None``, and an answer of the
#: same shapes. Anything else is reported to the script as a host error.
HostFunction = Callable[..., object]


class OmniPayload(Union):
    """Mirror of ``union OmniPayload``: ``{ double number; const char *ptr; }``."""

    _fields_ = [("number", c_double), ("ptr", c_void_p)]


class OmniValue(Structure):
    """Mirror of ``OmniValue``: ``{ uint32_t tag; uint32_t len; OmniPayload }``.

    ``len`` is the byte count of a string payload and zero for every other tag;
    it fills the hole that padding already left between a 4-byte tag and an
    8-byte payload, which is why strings cost no extra bytes.

    For a string, ``len`` is a sizing hint. ``omni_str_copy`` uses the engine's
    own header for the real count, so a stale ``len`` can read too few bytes but
    can never read past the end of a block.
    """

    _fields_ = [("tag", c_uint32), ("len", c_uint32), ("payload", OmniPayload)]

    @property
    def number(self) -> float:
        """The ``double`` arm. Only meaningful for the number and bool tags."""
        return self.payload.number

    @number.setter
    def number(self, value: float) -> None:
        self.payload.number = value

    @property
    def ptr(self) -> int | None:
        """The pointer arm, as an integer. Read it; never dereference it."""
        return self.payload.ptr

    @ptr.setter
    def ptr(self, value: int | None) -> None:
        self.payload.ptr = value

    def __repr__(self) -> str:  # pragma: no cover - debugging aid
        tag = TAGS.get(self.tag, self.tag)
        if self.tag == OMNI_STRING:
            return f"<OmniValue {tag} len={self.len} ptr={self.ptr!r}>"
        if self.tag in (OMNI_NUMBER, OMNI_BOOL):
            return f"<OmniValue {tag} {self.payload.number}>"
        return f"<OmniValue {tag}>"


#: ``int32_t (*)(void *, const OmniValue *, uint32_t, OmniValue *)`` -- the
#: shape the engine uses to enter host code.
#:
#: A ``CFUNCTYPE`` instance is the trampoline: C may call it at any later time,
#: which is only safe while the object itself is alive. ctypes does not register
#: the callback with the library, so nothing here holds a reference for us -- if
#: a binding's trampoline were garbage-collected, C would keep calling a freed
#: closure and CPython would crash in a place that looks nothing like the cause.
#: :class:`_HostBinding` below is the reference that keeps them alive.
HOST_FN = ctypes.CFUNCTYPE(
    c_int32,  # return: OMNI_HOST_OK or OMNI_HOST_ERR
    c_void_p,  # user_data -- always NULL from Python, see _HostBinding
    POINTER(OmniValue),  # args, argc of them, borrowed from the running call
    c_uint32,  # argc
    POINTER(OmniValue),  # out, read by the engine after we return
)


class OmniStats(Structure):
    """Mirror of ``OmniStats``: a size word, a microsecond timer, eight counters.

    ``last_exec_micros`` sits in the four bytes the C compiler padded after
    ``size``, so the struct stays 72 bytes and every other field keeps its
    offset -- which is why the ABI stayed at 4.
    """

    _fields_ = [
        ("size", c_uint32),
        ("last_exec_micros", c_uint32),
        ("execs", c_uint64),
        ("strings", c_uint64),
        ("arena_bytes_in_use", c_uint64),
        ("arena_bytes_reserved", c_uint64),
        ("arena_chunks", c_uint64),
        ("egress_bytes_in_use", c_uint64),
        ("egress_bytes_reserved", c_uint64),
        ("egress_high_water", c_uint64),
    ]


@dataclass(frozen=True)
class Stats:
    """The engine's memory counters, copied out of :class:`OmniStats`.

    ``arena_*`` is the per-execution scratch, bulk-freed at the start of every
    evaluation. ``egress_*`` is the block string results are promoted into; it
    rewinds instead of freeing, so ``egress_bytes_reserved`` is a high-water
    mark of the *largest* result a host has asked for, not a leak.
    """

    execs: int
    last_exec_micros: int
    strings: int
    arena_bytes_in_use: int
    arena_bytes_reserved: int
    arena_chunks: int
    egress_bytes_in_use: int
    egress_bytes_reserved: int
    egress_high_water: int


# Fail loudly at import time if the struct ever drifts from the Rust layout,
# instead of returning garbage numbers at run time. These are the same eleven
# facts the Rust side asserts and omni.h states with _Static_assert; a Python
# that disagrees says the SDK and the compiled library are out of sync.
_LAYOUT = [
    ("ctypes.sizeof(OmniValue)", ctypes.sizeof(OmniValue), 16),
    ("ctypes.sizeof(OmniPayload)", ctypes.sizeof(OmniPayload), 8),
    ("OmniValue.tag.offset", OmniValue.tag.offset, 0),
    ("OmniValue.len.offset", OmniValue.len.offset, 4),
    ("OmniValue.payload.offset", OmniValue.payload.offset, 8),
    ("ctypes.sizeof(OmniStats)", ctypes.sizeof(OmniStats), 72),
    ("OmniStats.size.offset", OmniStats.size.offset, 0),
    ("OmniStats.last_exec_micros.offset", OmniStats.last_exec_micros.offset, 4),
    ("OmniStats.execs.offset", OmniStats.execs.offset, 8),
    ("OmniStats.egress_high_water.offset", OmniStats.egress_high_water.offset, 64),
]
for _name, _actual, _expected in _LAYOUT:
    if _actual != _expected:  # pragma: no cover - layout regression
        raise RuntimeError(
            f"{_name} is {_actual}, expected {_expected}. The Python SDK and "
            "crates/omni-ffi disagree about the ABI; rebuild the library with "
            "cargo build -p omni-ffi."
        )
del _LAYOUT, _name, _actual, _expected


class OmniError(RuntimeError):
    """A program OmniEngine rejected, or a call a host made incorrectly.

    Attributes:
        status: the raw ``u32`` the ABI returned.
        kind: ``"compile"``, ``"runtime"``, ``"panic"``, ``"argument"``,
            ``"host"``, or ``"reentrant"``.
        message: the engine's own text, already copied out of the engine.
    """

    def __init__(self, status: int, message: str) -> None:
        super().__init__(f"{STATUS.get(status, status)}: {message}" if message else STATUS.get(status, str(status)))
        self.status = status
        self.kind = STATUS.get(status, f"status-{status}")
        self.message = message


class OmniLibraryError(OSError):
    """The shared library could not be found, or is not this ABI. Raised with
    the fix in it."""


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
    Then, so an installed wheel works with no configuration at all: the
    ``omni/_native`` directory a release wheel stages the artifact in, the
    package directory itself (a wheel that puts it beside the module), the tree
    the package sits in, and the tree the current command was run from.
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
    found.append(os.path.join(package_dir, "_native", name))  # bundled in a wheel
    found.append(os.path.join(package_dir, name))  # bundled beside the module
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
    """Load OmniEngine, declare the whole ABI on it, and check its ABI level.

    Raises:
        OmniLibraryError: if no artifact was found, or the one found speaks a
            different wire format. Every failure of this function so far has
            been "you have not rebuilt the library yet", so the message carries
            the build command.
    """
    tried = _candidate_paths()
    for path in tried:
        if path and os.path.isfile(path):
            return _checked(_bind(_cdll(path)), path)

    # Fall back to whatever the loader can resolve by bare name (a system-wide
    # install, or a directory on PATH).
    name = library_file_name()
    try:
        lib = ctypes.CDLL(name)
    except OSError as exc:
        raise OmniLibraryError(
            f"could not find {name}.\n"
            "A release wheel carries the engine in omni/_native; if that file\n"
            "is missing, reinstall the wheel for this platform.\n"
            "From a checkout, build it first:\n"
            "    cargo build -p omni-ffi\n"
            "The SDK then finds it on its own in any target/debug or\n"
            "target/release directory above this package or above the current\n"
            "working directory.\n"
            "To use a library somewhere else, set OMNI_ENGINE_LIBRARY to its\n"
            "full path, or OMNI_ENGINE_HOME to the project root.\n"
            f"Locations tried:\n{chr(10).join('  ' + p for p in tried)}\n"
            f"Loader said: {exc}"
        ) from exc
    return _checked(_bind(lib), name)


def _checked(lib: ctypes.CDLL, source: str) -> ctypes.CDLL:
    """Refuse a library whose value layout this module cannot read.

    A Phase 1 ``omni.dll`` has no ``omni_abi_version`` at all, so the lookup
    itself fails; that is the same problem as a version mismatch and gets the
    same answer, worded for the case the reader is most likely in.
    """
    try:
        declared = lib.omni_abi_version()
    except AttributeError as exc:  # pragma: no cover - stale artifact
        raise OmniLibraryError(
            f"{source} does not export omni_abi_version, so it predates the ABI "
            f"level this SDK needs ({OMNI_ABI_VERSION}). Rebuild it:\n"
            "    cargo build -p omni-ffi"
        ) from exc
    if declared != OMNI_ABI_VERSION:
        raise OmniLibraryError(
            f"{source} speaks ABI version {declared}; this SDK is written for "
            f"ABI version {OMNI_ABI_VERSION}. The value layout differs, so "
            "reading one as the other produces wrong numbers rather than an "
            "error. Rebuild the library, or install the SDK version that "
            "matches it:\n    cargo build -p omni-ffi"
        )
    return lib


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
    handle that looks valid until the first call that dereferences it, and a
    missing ``argtypes`` on ``omni_str_copy`` lets ctypes pass a Python ``str``
    where the ABI wants a writable buffer.
    """
    lib.omni_abi_version.argtypes = []
    lib.omni_abi_version.restype = c_uint32

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

    # Two arrays in, two arrays out. POINTER(c_char_p) is the array of source
    # addresses; the lengths are a separate array because no source is required
    # to be NUL-terminated. Both output arrays are caller-owned and sized by the
    # caller, which is why the count is passed rather than inferred.
    lib.omni_exec_many.argtypes = [
        c_void_p,
        POINTER(c_char_p),
        POINTER(c_size_t),
        c_size_t,
        POINTER(OmniValue),
        POINTER(c_uint32),
    ]
    lib.omni_exec_many.restype = c_uint32

    # Additive in ABI 4. A library built before concurrent dispatch still serves
    # every other call; RawEngine.exec_parallel() falls back to omni_exec_many()
    # when this symbol is absent rather than refusing to run.
    if hasattr(lib, "omni_exec_parallel"):
        lib.omni_exec_parallel.argtypes = [
            c_void_p,
            POINTER(c_char_p),
            POINTER(c_size_t),
            c_size_t,
            c_uint32,
            POINTER(OmniValue),
            POINTER(c_uint32),
        ]
        lib.omni_exec_parallel.restype = c_uint32

    lib.omni_batch_error.argtypes = [c_void_p, c_size_t, POINTER(ctypes.c_char), c_size_t]
    lib.omni_batch_error.restype = c_size_t

    # buf is POINTER(c_char), not c_char_p: the ABI writes into it, and a
    # one-way c_char_p would let ctypes hand out a read-only pointer to
    # immutable Python bytes.
    lib.omni_str_copy.argtypes = [c_void_p, POINTER(OmniValue), POINTER(ctypes.c_char), c_size_t]
    lib.omni_str_copy.restype = c_size_t

    lib.omni_define_string.argtypes = [c_void_p, c_char_p, c_size_t, c_char_p, c_size_t]
    lib.omni_define_string.restype = c_uint32

    # The callback parameter is declared as the CFUNCTYPE itself, so ctypes
    # checks the trampoline's shape at the call rather than letting any integer
    # through and faulting inside the engine.
    lib.omni_register_host_fn.argtypes = [c_void_p, c_char_p, c_size_t, c_uint32, HOST_FN, c_void_p]
    lib.omni_register_host_fn.restype = c_uint32

    lib.omni_unregister_host_fn.argtypes = [c_void_p, c_char_p, c_size_t]
    lib.omni_unregister_host_fn.restype = c_size_t

    lib.omni_host_fn_count.argtypes = [c_void_p]
    lib.omni_host_fn_count.restype = c_size_t

    lib.omni_debug_stats.argtypes = [c_void_p, POINTER(OmniStats)]
    lib.omni_debug_stats.restype = c_uint32

    lib.omni_last_error.argtypes = [c_void_p, POINTER(ctypes.c_char), c_size_t]
    lib.omni_last_error.restype = c_size_t

    lib.omni_debug_panic.argtypes = [c_void_p]
    lib.omni_debug_panic.restype = c_uint32

    # Additive in ABI 4, so it is declared only when the loaded library has it.
    # A library built before the update check existed still serves every other
    # call; check_update() below turns the missing symbol into one clear error
    # at the moment someone asks for an update, not at import time.
    if hasattr(lib, "omni_check_update"):
        lib.omni_check_update.argtypes = [
            c_char_p,
            c_size_t,
            POINTER(ctypes.c_char),
            c_size_t,
        ]
        lib.omni_check_update.restype = c_int32
    return lib


def check_update(manifest: str | bytes, *, version_cap: int = 64) -> tuple[int, str]:
    """Compare a release manifest with the loaded engine.

    A pure comparison: no network, no download, no writes. The caller fetches
    the manifest; this asks the engine that owns the version and ABI numbers
    whether the manifest names a newer build.

    Args:
        manifest: the JSON document bytes (or ``str``).
        version_cap: bytes reserved for the version the manifest names.

    Returns:
        ``(status, version)``. ``status`` is one of the ``OMNI_UPDATE_*``
        constants: ``0`` up to date, ``1`` available, ``2`` ABI mismatch,
        ``3`` unreadable manifest, ``-1`` bad argument. ``version`` is the
        manifest's version once it parsed, and ``""`` otherwise.

    Raises:
        OmniLibraryError: the loaded library predates ``omni_check_update``.
    """
    return _check_update_on(load_library(), manifest, version_cap=version_cap)


def _check_update_on(
    lib: ctypes.CDLL, manifest: str | bytes, *, version_cap: int = 64
) -> tuple[int, str]:
    """[`check_update`] against a library the caller already loaded."""
    function = getattr(lib, "omni_check_update", None)
    if function is None:
        raise OmniLibraryError(
            "the loaded OmniEngine library predates the update check "
            "(omni_check_update). Rebuild it with:\n    cargo build -p omni-ffi"
        )
    data = manifest.encode("utf-8") if isinstance(manifest, str) else bytes(manifest)
    if len(data) > MAX_MANIFEST_LEN:
        return OMNI_UPDATE_BAD_ARG, ""
    capacity = max(version_cap, 0)
    buf = create_string_buffer(max(capacity, 1))
    status = function(data, len(data), buf, capacity)
    return status, buf.value.decode("utf-8", "replace")


# ---------------------------------------------------------------------------
# Host callbacks
# ---------------------------------------------------------------------------

#: Longest host error message the trampoline will carry. The engine has no
#: separate limit on a failure string, so this one exists to keep a traceback
#: printed inside an inner exception handler from becoming the result.
MAX_ERROR_TEXT = 4096


class _HostBinding:
    """One registered Python callable, plus the two objects C must be able to reach.

    ``trampoline`` is the ctypes closure whose address the engine holds. ctypes
    frees a closure when the callback object dies, so this instance owns it and
    :class:`RawEngine` owns this instance -- the only way the address the engine
    has stays valid for the life of the registration.

    ``keep`` is the buffer from the most recent call. The engine reads ``out``
    *after* this trampoline returns to C, and a local buffer would be gone by
    then, so the last one is held until the next call replaces it. That is a
    per-function slot, not a growing list: one callback returns one value at a
    time, and re-entrancy is refused by the ABI, so a call cannot interrupt
    itself here.

    ``user_data`` is always NULL. A ctypes closure already carries its own
    environment, so the ABI's slot has nothing to point at -- and pointing it at
    this instance would make two owners of the same trampoline.
    """

    __slots__ = ("name", "arity", "fn", "trampoline", "keep")

    def __init__(self, name: bytes, arity: int, fn: HostFunction) -> None:
        self.name = name
        self.arity = arity
        self.fn = fn
        self.trampoline = HOST_FN(self._call)
        self.keep: ctypes.Array | None = None

    def _call(self, _user_data, args, argc, out) -> int:  # noqa: ANN001 - ctypes
        """The trampoline's body: lower, call, write -- or fail as data."""
        try:
            values = [_lift(args[index]) for index in range(argc)]
            result = self.fn(*values)
            self._write(out, result)
        except BaseException as exc:  # noqa: BLE001 - the boundary must not unwind
            # Any exception, including KeyboardInterrupt, becomes an
            # OMNI_HOST_ERR message. Nothing may propagate through this frame:
            # ctypes would swallow it and return a zero, and the engine would
            # read a half-written ``out`` as a result. The host sees the failure
            # in the script's error text instead, which is the honest version.
            return self._fail(out, exc)
        return OMNI_HOST_OK

    def _write(self, out, result) -> None:  # noqa: ANN001 - ctypes
        target = out[0]  # a shadow over the engine's OmniValue; writes go through
        target.len = 0
        if result is None:
            target.tag = OMNI_NIL
            return
        if isinstance(result, bool):
            # Before the numeric test: True *is* an int in Python, and a boolean
            # answer should read as `true` in the script, not as 1.
            target.tag = OMNI_BOOL
            target.payload.number = 1.0 if result else 0.0
            return
        if isinstance(result, (int, float)):
            target.tag = OMNI_NUMBER
            target.payload.number = float(result)
            return
        if isinstance(result, (str, bytes, bytearray)):
            data = result.encode("utf-8") if isinstance(result, str) else bytes(result)
            if len(data) > MAX_STRING_LEN:
                raise ValueError(
                    f"a host function returns at most {MAX_STRING_LEN} string bytes, got {len(data)}"
                )
            buffer = create_string_buffer(data)  # len(data) + 1, NUL at the end
            target.tag = OMNI_STRING
            target.len = len(data)
            target.payload.ptr = ctypes.addressof(buffer)
            self.keep = buffer
            return
        raise TypeError(
            f"a host function must return a number, bool, str, bytes or None, "
            f"not {type(result).__name__}"
        )

    def _fail(self, out, exc: BaseException) -> int:  # noqa: ANN001 - ctypes
        message = f"{type(exc).__name__}: {exc}".encode("utf-8", "replace")[:MAX_ERROR_TEXT]
        buffer = create_string_buffer(message)
        target = out[0]
        target.tag = OMNI_STRING
        target.len = len(message)
        target.payload.ptr = ctypes.addressof(buffer)
        self.keep = buffer
        return OMNI_HOST_ERR


def _lift(value: OmniValue) -> object:
    """Read one argument the engine handed us, from its borrowed bytes."""
    if value.tag == OMNI_NUMBER:
        return value.payload.number
    if value.tag == OMNI_BOOL:
        return value.payload.number != 0.0
    if value.tag == OMNI_STRING:
        # string_at, not omni_str_copy: the copy helper takes the engine handle,
        # and the engine is inside the call that invoked us, so it would answer
        # OMNI_ERR_REENTRANT with a zero-length read. The bytes are readable --
        # that is the promise the ABI makes and this is where it is spent.
        if not value.ptr or value.len == 0:
            return ""
        return string_at(value.ptr, value.len).decode("utf-8", "replace")
    return None  # nil and undef both carry nothing a Python function can use


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

    __slots__ = (
        "_lib",
        "_handle",
        "_host_fns",
        "_batch_ptrs",
        "_batch_lengths",
        "_batch_values",
        "_batch_statuses",
        "_batch_capacity",
        "_batch_keep",
        "_string_buf",
        "_string_cap",
    )

    def __init__(self, lib: ctypes.CDLL | None = None) -> None:
        self._lib = lib if lib is not None else load_library()
        handle = self._lib.omni_engine_new()
        if not handle:
            raise OmniLibraryError("omni_engine_new returned a null handle")
        self._handle = handle
        # Name (as registered bytes) -> binding. This dict is the only owner of
        # the ctypes closures whose addresses the engine holds.
        self._host_fns: dict[bytes, _HostBinding] = {}
        # The four arrays a batch hands the ABI, sized to the largest batch this
        # engine has ever been asked for and reused after that. Building them is
        # four allocations and a C-side fill per call otherwise, which is the
        # batch's own overhead -- measured, and the reason it is paid once.
        self._batch_ptrs = None
        self._batch_lengths = None
        self._batch_values = None
        self._batch_statuses = None
        self._batch_capacity = 0
        # The encoded sources of the most recent batch, held so that no slot of
        # `_batch_ptrs` is ever an address whose bytes have already died.
        self._batch_keep = None
        # One growable destination for omni_str_copy. The bytes it hands back is
        # a fresh object, so the buffer itself is reusable -- and a per-result
        # allocation is the difference between a batch and a loop.
        self._string_buf = None
        self._string_cap = 0

    @property
    def lib(self) -> ctypes.CDLL:
        return self._lib

    def version(self) -> str:
        raw = self._lib.omni_version()
        return raw.decode("utf-8", "replace") if raw else ""

    def abi_version(self) -> int:
        """Wire format the loaded library reports; :func:`load_library` checked it."""
        return self._lib.omni_abi_version()

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

    def exec_many(self, sources: Sequence[str | bytes]) -> tuple[int, list[OmniValue], list[int]]:
        """Evaluate every source in one crossing; never raises on a script error.

        Returns ``(first_failing_status, values, statuses)``. The two lists have
        one entry per source, in order, including for items that failed -- a
        failed item's value is ``OMNI_UNDEF``.

        Every string in ``values`` is readable *now*, because the batch shares
        one egress rewind. They all expire on the next evaluation of either
        kind, so a caller that keeps them must copy them out first, which is
        what :meth:`read_string` is for.

        Raises:
            OmniError: more than :data:`MAX_BATCH` sources, or one source longer
                than :data:`MAX_SOURCE_LEN`. Both are checked here, before any
                pointer is handed over, so the engine never sees a bad array.
        """
        prepared = self._prepare_batch(sources)
        if prepared is None:
            return OMNI_OK, [], []
        count, ptrs, lengths, values, statuses = prepared
        status = self._lib.omni_exec_many(self._handle, ptrs, lengths, count, values, statuses)
        return status, [values[index] for index in range(count)], [statuses[index] for index in range(count)]

    def exec_parallel(
        self,
        sources: Sequence[str | bytes],
        max_threads: int = 0,
    ) -> tuple[int, list[OmniValue], list[int]]:
        """Evaluate every source on a worker group; never raises on a script error.

        The call is synchronous and writes results in source order, so the
        return value has the same shape and lifetime as :meth:`exec_many`.
        ``max_threads`` of 0 asks the engine for its automatic choice; 1 runs
        the sequential path. A failing item does not stop its neighbours.

        A library built before this export existed has no
        ``omni_exec_parallel`` symbol; this method then runs ``omni_exec_many``
        instead, so the call keeps working with the same answers and only the
        concurrency is missing.

        Raises:
            OmniError: the same batch limits as :meth:`exec_many`, or a
                ``max_threads`` that is not a non-negative 32-bit integer.
        """
        if (
            not isinstance(max_threads, int)
            or isinstance(max_threads, bool)
            or max_threads < 0
            or max_threads > 0xFFFFFFFF
        ):
            raise OmniError(
                OMNI_ERR_ARG,
                f"max_threads must be a uint32, not {max_threads!r}",
            )
        prepared = self._prepare_batch(sources)
        if prepared is None:
            return OMNI_OK, [], []
        count, ptrs, lengths, values, statuses = prepared
        parallel = getattr(self._lib, "omni_exec_parallel", None)
        if parallel is None:
            status = self._lib.omni_exec_many(
                self._handle, ptrs, lengths, count, values, statuses
            )
        else:
            status = parallel(
                self._handle, ptrs, lengths, count, max_threads, values, statuses
            )
        return status, [values[index] for index in range(count)], [statuses[index] for index in range(count)]

    def _prepare_batch(
        self, sources: Sequence[str | bytes]
    ) -> tuple[int, Any, Any, Any, Any] | None:
        """Encode and marshal one batch; ``None`` means there was nothing to run.

        Shared by the sequential and parallel entry points so both validate the
        count, own the encoded bytes for the ABI call, and start from zeroed
        statuses in exactly one place.
        """
        encoded = [self._encode(source) for source in sources]
        count = len(encoded)
        if count > MAX_BATCH:
            raise OmniError(
                OMNI_ERR_ARG,
                f"a batch carries at most {MAX_BATCH} programs, not {count}",
            )
        if count == 0:
            return None
        self._grow_batch(count)
        ptrs, lengths, values, statuses = (
            self._batch_ptrs,
            self._batch_lengths,
            self._batch_values,
            self._batch_statuses,
        )
        # `encoded` owns the bytes these slots point into, and
        # `_batch_keep` holds it for the ABI call and until the next one --
        # ctypes does not keep a reference on your behalf, and a reused array
        # would otherwise outlive the buffers it was filled with.
        self._batch_keep = encoded
        for index, data in enumerate(encoded):
            ptrs[index] = data
            lengths[index] = len(data)
            # Cleared here rather than allocated: an argument error refuses the
            # whole call without writing a single status, and a stale non-zero
            # from the previous batch would read back as this batch's failure.
            statuses[index] = 0
        return count, ptrs, lengths, values, statuses

    def _grow_batch(self, count: int) -> None:
        """Make the reused batch arrays hold at least `count` sources.

        Sizes double from 8, so a host that walks its queue in slightly larger
        steps reallocates logarithmically, and the capacity can never pass
        :data:`MAX_BATCH` because `exec_many` refuses above it.
        """
        if count <= self._batch_capacity:
            return
        size = 8
        while size < count:
            size *= 2
        self._batch_ptrs = (c_char_p * size)()
        self._batch_lengths = (c_size_t * size)()
        self._batch_values = (OmniValue * size)()
        self._batch_statuses = (c_uint32 * size)()
        self._batch_capacity = size

    def batch_error(self, index: int, cap: int = 4096) -> str:
        """The rendered error of item ``index`` of the most recent batch.

        ``""`` for an item that succeeded, for an index past the batch, and after
        a single :meth:`exec` -- which ends the batch's currency. The engine's own
        :meth:`last_error` holds only the *final* item's message, which is why
        this exists.
        """
        buf = ctypes.create_string_buffer(cap)
        written = self._lib.omni_batch_error(self._handle, index, buf, cap)
        return buf.raw[:written].decode("utf-8", "replace")

    def read_string(self, value: OmniValue) -> bytes:
        """Copy a string result into Python storage, through the ABI.

        Returns ``b""`` for a value that is not a string, and for the empty
        string -- the tag is what distinguishes those, and only this engine's
        header decides how many bytes arrive. The buffer is sized from
        ``value.len``, which is the hint the ABI offers; a short return means
        the address was stale, not that the copy failed.
        """
        if value.tag != OMNI_STRING:
            return b""
        size = value.len
        if size == 0:
            return b""
        # create_string_buffer(n) allocates n + 1 and zeroes it, so there is
        # always a byte past the request to prove the ABI wrote no further. The
        # buffer is this engine's, grown when a result needs more than it holds:
        # the bytes returned below is a fresh object, so nothing handed to a
        # caller points into it.
        if self._string_cap < size:
            self._string_buf = ctypes.create_string_buffer(size)
            self._string_cap = size
        written = self._lib.omni_str_copy(self._handle, byref(value), self._string_buf, size)
        return ctypes.string_at(ctypes.addressof(self._string_buf), written)

    def define_string(self, name: str | bytes, payload: str | bytes) -> int:
        """Bind a host string; returns the status. Never raises on error.

        Both buffers are encoded and bounded here, before the ABI sees a
        pointer, so an over-long name is a Python ``OmniError`` rather than a
        rejection that has to be decoded from a status code. A name that is not
        valid UTF-8 in *bytes* form still reaches the ABI, which reports it as
        ``OMNI_ERR_ARG``.
        """
        encoded_name = self._encode_bound("name", name, MAX_IDENTIFIER_LEN)
        encoded_payload = self._encode_bound("payload", payload, MAX_STRING_LEN)
        return self._lib.omni_define_string(
            self._handle,
            encoded_name,
            len(encoded_name),
            encoded_payload,
            len(encoded_payload),
        )

    def register_host_fn(self, name: str | bytes, arity: int, fn: HostFunction) -> int:
        """Let scripts call ``fn`` as ``name(...)``; returns the status.

        The registration survives every later evaluation, and so does this
        object's reference to the closure: :mod:`ctypes` frees a callback the
        moment its Python object dies, and the engine would go on calling the
        address it was given.

        Raises:
            OmniError: the name is empty or longer than
                :data:`MAX_IDENTIFIER_LEN` bytes, ``fn`` is not callable, or
                ``arity`` is not a number of arguments the ABI can carry.
        """
        encoded_name = self._encode_bound("name", name, MAX_IDENTIFIER_LEN)
        if not callable(fn):
            # Refused here rather than at the first call. A ctypes closure can be
            # built around anything, so an uncalled mistake would otherwise surface
            # as a host error inside a script several evaluations later.
            raise OmniError(OMNI_ERR_ARG, f"a host function must be callable, not {type(fn).__name__}")
        if not isinstance(arity, int) or isinstance(arity, bool):
            raise OmniError(OMNI_ERR_ARG, f"arity must be an int, not {type(arity).__name__}")
        if not 0 <= arity <= MAX_ARGS:
            raise OmniError(
                OMNI_ERR_ARG,
                f"a host function takes between 0 and {MAX_ARGS} arguments, not {arity}",
            )
        binding = _HostBinding(encoded_name, arity, fn)
        status = self._lib.omni_register_host_fn(
            self._handle,
            encoded_name,
            len(encoded_name),
            arity,
            binding.trampoline,
            None,  # user_data: a ctypes closure carries its own environment
        )
        if status == OMNI_OK:
            # Only on success, and only now: the engine has already forgotten the
            # previous binding for this name, so dropping ours cannot strand a
            # trampoline it still means to call.
            self._host_fns[encoded_name] = binding
        return status

    def unregister_host_fn(self, name: str | bytes) -> bool:
        """Drop a host function; True if the engine had one.

        The engine releases the callback pointer here, which is the signal that
        this module may release the closure -- so the two halves always go
        together, including when the status is a silent ``0``.
        """
        encoded_name = self._encode_bound("name", name, MAX_IDENTIFIER_LEN)
        removed = self._lib.omni_unregister_host_fn(self._handle, encoded_name, len(encoded_name))
        if removed:
            self._host_fns.pop(encoded_name, None)
        return bool(removed)

    def host_fn_count(self) -> int:
        """Registrations the engine holds. Zero is what a teardown test wants."""
        return self._lib.omni_host_fn_count(self._handle)

    def stats(self) -> Stats:
        """Read the memory counters, verifying the layout the library wrote."""
        out = OmniStats()
        status = self._lib.omni_debug_stats(self._handle, byref(out))
        if status != OMNI_OK:
            raise OmniError(status, self.last_error())
        if out.size != ctypes.sizeof(OmniStats):
            raise OmniError(
                OMNI_ERR_ARG,
                f"the library says OmniStats is {out.size} bytes; this SDK reads "
                f"{ctypes.sizeof(OmniStats)}",
            )
        return Stats(
            execs=out.execs,
            last_exec_micros=out.last_exec_micros,
            strings=out.strings,
            arena_bytes_in_use=out.arena_bytes_in_use,
            arena_bytes_reserved=out.arena_bytes_reserved,
            arena_chunks=out.arena_chunks,
            egress_bytes_in_use=out.egress_bytes_in_use,
            egress_bytes_reserved=out.egress_bytes_reserved,
            egress_high_water=out.egress_high_water,
        )

    def last_error(self, cap: int = 4096) -> str:
        buf = ctypes.create_string_buffer(cap)
        written = self._lib.omni_last_error(self._handle, buf, cap)
        return buf.raw[:written].decode("utf-8", "replace")

    def debug_panic(self) -> int:
        return self._lib.omni_debug_panic(self._handle)

    def close(self) -> None:
        if self._handle:
            # Unregister before freeing: the engine forgets every trampoline it
            # was handed, and this dict stays the last owner of them until it
            # does. Releasing a closure the engine still holds is the one way a
            # Python host can make omni_exec() call freed memory.
            for encoded_name in list(self._host_fns):
                self._lib.omni_unregister_host_fn(self._handle, encoded_name, len(encoded_name))
            self._host_fns.clear()
            self._lib.omni_engine_free(self._handle)
            self._handle = None

    @staticmethod
    def _encode(source: str | bytes) -> bytes:
        """Encode to UTF-8 and bound the length the way the ABI does.

        The bound is read from the module at call time, not captured as a
        default argument, so a test can lower ``MAX_SOURCE_LEN`` and exercise
        the guard without allocating 256 MiB of source.
        """
        data = source.encode("utf-8") if isinstance(source, str) else bytes(source)
        if len(data) > MAX_SOURCE_LEN:
            raise OmniError(
                OMNI_ERR_ARG,
                f"source is {len(data)} bytes; the ABI accepts at most {MAX_SOURCE_LEN}",
            )
        return data

    @staticmethod
    def _encode_bound(name: str, value: str | bytes, limit: int) -> bytes:
        """Encode one host-supplied buffer and bound it the way the ABI does.

        Same call-time rule as :meth:`_encode`: the limit is read here, so a
        test can lower :data:`MAX_IDENTIFIER_LEN` without a 16 MiB payload.
        """
        data = value.encode("utf-8") if isinstance(value, str) else bytes(value)
        bound = limit
        if len(data) > bound:
            raise OmniError(
                OMNI_ERR_ARG,
                f"{name} is {len(data)} bytes; the ABI accepts at most {bound}",
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
