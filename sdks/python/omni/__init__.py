"""OmniEngine from Python, with no third-party dependencies.

``omni`` wraps OmniEngine's C ABI using only :mod:`ctypes`, so installing it
pulls in nothing and it works on any CPython that can load a shared library.

    import omni

    with omni.Engine() as engine:
        engine.evaluate("let answer = 40 + 2")   # -> 42.0
        engine.run("20 / 3").value               # -> 6.666666666666667
        engine.run(r'"omni" + " " + "engine"').value   # -> 'omni engine'

``evaluate`` and ``run`` evaluate OmniEngine source only. OmniEngine is, for
now, an expression language with numbers, booleans and strings: there is no
host-code execution to escape into, and nothing here calls Python's ``eval``.

Everything this package hands back is Python data. A string result is copied
out of the engine by ``omni_str_copy`` before it becomes a ``str``, so no
object you can hold refers to memory the engine rewinds on the next call.

    engine.define_string("greeting", "hello")     # visible to the program
    engine.run("greeting + \", then some\"").value
    engine.stats().egress_bytes_reserved          # what the buffers cost

Build the library once and the SDK finds it on its own:

    cargo build -p omni-ffi

To point at a library somewhere else, set ``OMNI_ENGINE_LIBRARY`` to the file
or ``OMNI_ENGINE_HOME`` to the project root. An artifact built from an older
commit of this repository is refused at import, with the same instruction in
the error, because a changed value layout reads the wrong bytes silently.

An engine handle is not thread-safe. Create one per thread, or serialise the
calls yourself.
"""

from __future__ import annotations

import ctypes
from dataclasses import dataclass

from ._ffi import (
    MAX_IDENTIFIER_LEN,
    MAX_SOURCE_LEN,
    MAX_STRING_LEN,
    OMNI_ABI_VERSION,
    OMNI_ERR_RUNTIME,
    OMNI_OK,
    STATUS,
    TAGS,
    OmniError,
    OmniLibraryError,
    OmniValue,
    RawEngine,
    Stats,
    library_file_name,
)

__all__ = [
    "Engine",
    "Result",
    "Stats",
    "OmniError",
    "OmniLibraryError",
    "library_file_name",
    "status_name",
    "tag_name",
    "MAX_SOURCE_LEN",
    "MAX_STRING_LEN",
    "MAX_IDENTIFIER_LEN",
    "OMNI_ABI_VERSION",
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

    ``tag`` is ``"number"``, ``"bool"``, ``"string"``, ``"nil"``, or ``"undef"``.
    ``number`` is the raw double, meaningful for numbers and booleans.
    ``text`` is the exact bytes of a string result, already copied out of the
    engine; empty for every other tag.
    ``value`` is the same thing as a Python object: ``float``, ``bool``, ``str``
    or ``None``.

    A string may contain a NUL. ``text`` keeps it, and so does ``value`` -- the
    engine's strings are length-delimited, not C strings.
    """

    tag: str
    number: float = 0.0
    text: bytes = b""

    @property
    def value(self) -> object:
        if self.tag == "number":
            return self.number
        if self.tag == "bool":
            return self.number != 0.0
        if self.tag == "string":
            # errors="replace", not "strict": bytes this package returns are
            # already valid UTF-8 by construction, so a decode failure would be
            # an engine bug -- and losing the whole result to a
            # UnicodeDecodeError is worse than one U+FFFD to report upstream.
            return self.text.decode("utf-8", "replace")
        return None  # nil and undef carry no payload

    def __str__(self) -> str:
        return repr(self.value)


def _convert(raw: OmniValue, text: bytes = b"") -> Result:
    """Build a Result from the struct the ABI filled.

    ``number`` is read only for the tags that mean a double. For a string the
    same eight bytes are an address, and reporting it as a float would be
    meaningless noise in every log that prints a Result.
    """
    tag = tag_name(raw.tag)
    number = raw.number if tag in ("number", "bool") else 0.0
    return Result(tag=tag, number=number, text=text)


class Engine:
    """A private OmniEngine interpreter.

    Everything this class returns is Python data. Nothing handed out is a
    pointer: string results are copied at the boundary, because the engine
    rewinds its egress block at the start of the next evaluation.
    """

    __slots__ = ("_raw", "_last_status")

    def __init__(self, library: ctypes.CDLL | None = None) -> None:
        """Create an engine.

        Args:
            library: an already-loaded, already-declared library handle, for a
                caller that wants to control discovery itself. Normally omitted.

        Raises:
            OmniLibraryError: no shared library was found -- build it -- or the
                one found speaks a different ABI version than this SDK.
        """
        self._raw: RawEngine | None = RawEngine(library)
        self._last_status = OMNI_OK

    @property
    def version(self) -> str:
        """Version reported by the engine library, not by this SDK."""
        return self._require().version()

    @property
    def abi_version(self) -> int:
        """Wire format of the loaded library. :func:`load_library` enforced it."""
        return self._require().abi_version()

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
        return _convert(value, raw.read_string(value))

    def evaluate(self, source: str) -> float:
        """Evaluate ``source`` requiring a numeric result.

        This is the path a caller takes when a non-number is a bug in *their*
        program, so :class:`OmniError` is raised instead of a tag returned. A
        string is a non-number: use :meth:`run` for those.
        """
        raw = self._require()
        status, number = raw.exec_number(source)
        self._last_status = status
        if status != OMNI_OK:
            raise OmniError(status, self._message(raw, status))
        return number

    def string(self, source: str) -> str:
        """Evaluate ``source`` requiring a string result.

        The mirror of :meth:`evaluate`. It exists because a caller who wants
        text and gets a number has the same bug in the other direction, and
        ``run(source).value`` hands back an ``object`` that still needs a
        ``isinstance`` check.

        Raises:
            OmniError: the program failed, or did not produce a string.
        """
        result = self.run(source)
        if result.tag != "string":
            raise OmniError(
                OMNI_ERR_RUNTIME,
                f"the program did not evaluate to a string (it produced a {result.tag})",
            )
        return result.value  # type: ignore[return-value]  # tag checked above

    def try_run(self, source: str) -> tuple[int, Result | None, str]:
        """Evaluate without exceptions: ``(status, result, message)``.

        For REPLs and editors, where a syntax error is a normal event.
        """
        raw = self._require()
        status, value = raw.exec(source)
        self._last_status = status
        if status != OMNI_OK:
            return status, None, self._message(raw, status)
        return status, _convert(value, raw.read_string(value)), ""

    def define_string(self, name: str, value: str) -> None:
        """Bind ``name`` to a string the *host* owns.

        The engine copies ``value`` into storage that lives as long as the
        binding, so the program can read it on any later call -- unlike a result
        string, which is only valid until the next evaluation. Redefining
        ``name`` replaces the binding, and a Result kept from before then reads
        back as ``b""`` rather than as freed memory.

        ``name`` is at most :data:`MAX_IDENTIFIER_LEN` UTF-8 bytes; the payload
        is at most :data:`MAX_STRING_LEN`.

        Raises:
            OmniError: the name or payload was rejected -- too long, an empty
                name, not valid UTF-8, or not a legal identifier.
        """
        raw = self._require()
        status = raw.define_string(name, value)
        self._last_status = status
        if status != OMNI_OK:
            raise OmniError(status, self._message(raw, status) or f"the ABI rejected the binding {name!r}")

    def stats(self) -> Stats:
        """The engine's memory counters: executions, strings built, bytes held.

        Read them after a benchmark to tell a high-water mark from a leak. The
        arena is bulk-freed per execution and the egress block rewinds, so
        ``*_bytes_reserved`` only ever grows to the largest single request.
        """
        return self._require().stats()

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
