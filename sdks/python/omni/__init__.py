"""OmniEngine from Python, with no third-party dependencies.

``omni`` wraps OmniEngine's C ABI using only :mod:`ctypes`, so installing it
pulls in nothing and it works on any CPython that can load a shared library.

    import omni

    with omni.Engine() as engine:
        engine.evaluate("let answer = 40 + 2")   # -> 42.0
        engine.run("20 / 3").value               # -> 6.666666666666667

``evaluate`` and ``run`` evaluate OmniEngine source only. OmniEngine is, for
now, an arithmetic expression language: there is no host-code execution to
escape into, and nothing here calls Python's ``eval``.

Build the library once and the SDK finds it on its own:

    cargo build -p omni-ffi

To point at a library somewhere else, set ``OMNI_ENGINE_LIBRARY`` to the file
or ``OMNI_ENGINE_HOME`` to the project root.

An engine handle is not thread-safe. Create one per thread, or serialise the
calls yourself.
"""

from __future__ import annotations

import ctypes
from dataclasses import dataclass

from ._ffi import (
    MAX_SOURCE_LEN,
    OMNI_ERR_RUNTIME,
    OMNI_OK,
    STATUS,
    TAGS,
    OmniError,
    OmniLibraryError,
    OmniValue,
    RawEngine,
    library_file_name,
)

__all__ = [
    "Engine",
    "Result",
    "OmniError",
    "OmniLibraryError",
    "library_file_name",
    "status_name",
    "tag_name",
    "MAX_SOURCE_LEN",
    "__version__",
]

#: Version of this SDK, kept equal to the workspace version of OmniEngine.
__version__ = "0.1.0"


def status_name(status: int) -> str:
    """Human-readable name for a status code returned by the ABI."""
    return STATUS.get(status, f"status-{status}")


def tag_name(tag: int) -> str:
    """Human-readable name for a value tag returned by the ABI."""
    return TAGS.get(tag, f"tag-{tag}")


@dataclass(frozen=True)
class Result:
    """What one evaluation produced.

    ``tag`` is ``"number"``, ``"bool"``, ``"nil"``, or ``"undef"``.
    ``number`` is the raw double, meaningful for numbers and booleans.
    ``value`` is the same thing as a Python object.
    """

    tag: str
    number: float

    @property
    def value(self) -> object:
        if self.tag == "number":
            return self.number
        if self.tag == "bool":
            return self.number != 0.0
        return None  # nil and undef carry no payload

    def __str__(self) -> str:
        return repr(self.value)


def _convert(raw: OmniValue) -> Result:
    return Result(tag=tag_name(raw.tag), number=raw.number)


class Engine:
    """A private OmniEngine interpreter.

    Everything this class returns is Python data. Nothing handed out is a
    pointer, because the engine reclaims its whole arena on the next call.
    """

    __slots__ = ("_raw", "_last_status")

    def __init__(self, library: ctypes.CDLL | None = None) -> None:
        """Create an engine.

        Args:
            library: an already-loaded, already-declared library handle, for a
                caller that wants to control discovery itself. Normally omitted.

        Raises:
            OmniLibraryError: no shared library was found -- build it.
        """
        self._raw: RawEngine | None = RawEngine(library)
        self._last_status = OMNI_OK

    @property
    def version(self) -> str:
        """Version reported by the engine library, not by this SDK."""
        return self._require().version()

    @property
    def last_status(self) -> int:
        """Status code of the most recent call, for hosts that dislike exceptions."""
        return self._last_status

    @property
    def last_error(self) -> str:
        """The engine's message from the most recent failure, or ``""``.

        A successful evaluation clears it, so this is never a stale error.
        """
        return self._require().last_error()

    def run(self, source: str) -> Result:
        """Evaluate ``source`` and return a :class:`Result`.

        Raises:
            OmniError: the program failed to compile or run, or the call used
                arguments the ABI rejects.
        """
        raw = self._require()
        status, value = raw.exec(source)
        self._last_status = status
        if status != OMNI_OK:
            raise OmniError(status, self._message(raw, status))
        return _convert(value)

    def evaluate(self, source: str) -> float:
        """Evaluate ``source`` requiring a numeric result.

        This is the path a caller takes when a non-number is a bug in *their*
        program, so :class:`OmniError` is raised instead of a tag returned.
        """
        raw = self._require()
        status, number = raw.exec_number(source)
        self._last_status = status
        if status != OMNI_OK:
            raise OmniError(status, self._message(raw, status))
        return number

    def try_run(self, source: str) -> tuple[int, Result | None, str]:
        """Evaluate without exceptions: ``(status, result, message)``.

        For REPLs and editors, where a syntax error is a normal event.
        """
        raw = self._require()
        status, value = raw.exec(source)
        self._last_status = status
        if status != OMNI_OK:
            return status, None, self._message(raw, status)
        return status, _convert(value), ""

    def close(self) -> None:
        """Release the engine. Idempotent."""
        if self._raw is not None:
            self._raw.close()
            self._raw = None

    @staticmethod
    def _message(raw: RawEngine, status: int) -> str:
        """Engine text, with a truthful fallback when the ABI has none.

        ``exec_number`` can report a type failure the engine itself did not
        produce, so the engine's own message is legitimately empty there.
        """
        text = raw.last_error()
        if text:
            return text
        if status == OMNI_ERR_RUNTIME:
            return "the program did not evaluate to a number"
        return ""

    def _require(self) -> RawEngine:
        if self._raw is None:
            raise OmniError(4, "this Engine is closed")
        return self._raw

    def __enter__(self) -> "Engine":
        return self

    def __exit__(self, *exc: object) -> bool:
        self.close()
        return False

    def __repr__(self) -> str:
        state = "closed" if self._raw is None else f"v{self._raw.version()}"
        return f"<omni.Engine {state}>"
