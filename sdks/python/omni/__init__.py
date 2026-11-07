"""OmniEngine from Python, with no third-party dependencies.

``omni`` wraps OmniEngine's C ABI using only :mod:`ctypes`, so installing it
pulls in nothing and it works on any CPython that can load a shared library.

    import omni

    with omni.Engine() as engine:
        engine.evaluate("let answer = 40 + 2")   # -> 42.0
        engine.run("20 / 3").value               # -> 6.666666666666667
        engine.run(r'"omni" + " " + "engine"').value   # -> 'omni engine'

``evaluate`` and ``run`` evaluate OmniEngine source only. OmniEngine is an
expression language with numbers, booleans and strings; it has no loops, no
I/O and no way to reach Python except a function you registered yourself.
Nothing here calls Python's ``eval``.

    engine.define_string("greeting", "hello")     # visible to the program
    engine.run("greeting + \", then some\"").value
    engine.stats().egress_bytes_reserved          # what the buffers cost

A Python callable becomes a name the script can call:

    engine.register_function("square", lambda x: x * x, arity=1)
    engine.run("square(6) + 1").value              # -> 37.0
    engine.run('shout("hi")').value                # -> 'HI!'  (yours to define)

Arguments arrive as ``float``, ``bool``, ``str`` or ``None`` and the return
value travels back the same way. An exception in the callable is not a crash
and not a silent zero: it becomes an :class:`OmniError` whose kind is
``"host"``, carrying the message to the line of the call that caused it.

Everything this package hands back is Python data. A string result is copied
out of the engine by ``omni_str_copy`` before it becomes a ``str``, so no
object you can hold refers to memory the engine rewinds on the next call, and
the same holds for a string argument inside a registered function.

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
from collections.abc import Sequence
from dataclasses import dataclass

from ._ffi import (
    MAX_ARGS,
    MAX_BATCH,
    MAX_CALL_DEPTH,
    MAX_IDENTIFIER_LEN,
    MAX_SOURCE_LEN,
    MAX_STRING_LEN,
    OMNI_ABI_VERSION,
    OMNI_ERR_REENTRANT,
    OMNI_ERR_RUNTIME,
    OMNI_OK,
    STATUS,
    TAGS,
    HostFunction,
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
    "HostFunction",
    "library_file_name",
    "status_name",
    "tag_name",
    "MAX_SOURCE_LEN",
    "MAX_STRING_LEN",
    "MAX_IDENTIFIER_LEN",
    "MAX_ARGS",
    "MAX_BATCH",
    "MAX_CALL_DEPTH",
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

    def eval_str(self, source: str) -> object:
        """Evaluate ``source`` and return plain Python data, already unwrapped.

        The one-call form for a REPL or a template engine: the result is an
        ``int``, ``float``, ``str``, ``bool`` or ``None``, never a
        :class:`Result` and never an ``object`` that still needs a tag check.

        A whole number becomes ``int`` and everything else stays ``float``. That
        is a value-preserving rule within the safe-integer range and a widening
        one outside it, where ``int`` holds the exact value ``float`` displayed.

        Raises:
            OmniError: the program failed. A program that simply produces a
                value of another type is not an error here -- that is the point.
        """
        value = self.run(source).value
        if isinstance(value, float) and value.is_integer():
            return int(value)
        return value

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

    def run_many(self, sources: Sequence[str]) -> list[Result]:
        """Evaluate every source in one crossing and return one Result each.

        The point is the crossing, not the convenience: a loop of :meth:`run`
        pays the FFI round trip and the engine's per-execution reclamation once
        per program, and this pays both once for the whole list.

        Each source is an independent program. A ``let`` in one is not visible
        to the next, exactly as two :meth:`run` calls would behave.

        Every string result is copied out before this returns, so the list is
        ordinary Python data and nothing in it expires.

        Raises:
            OmniError: the first item that failed, named by its index. Items
                after it still ran -- use :meth:`try_run_many` to see them all.
        """
        items = self._batch(sources)
        for index, (status, _result, message) in enumerate(items):
            if status != OMNI_OK:
                raise OmniError(status, f"item {index} of {len(items)}: {message}")
        return [result for _status, result, _message in items]  # type: ignore[return-value]

    def try_run_many(self, sources: Sequence[str]) -> list[tuple[int, Result | None, str]]:
        """Evaluate a batch without exceptions: one ``(status, result, message)``
        per source, in order.

        The per-item shape of :meth:`try_run`. A failing item carries its own
        status and the engine's message for *that* item -- not the message of the
        last one, which is all ``last_error`` can offer after a batch.

        An empty sequence returns an empty list and expires nothing: polling an
        empty queue does not invalidate a Result you are still holding.

        Raises:
            OmniError: the call itself was refused -- too many sources, an
                over-long source, a closed engine. A program that fails is data
                here; a call that was built wrong is not.
        """
        return self._batch(sources)

    def _batch(self, sources: Sequence[str]) -> list[tuple[int, Result | None, str]]:
        """One crossing, then every string copied out before anything else runs.

        The copy has to happen here and has to happen for every item: the batch
        shares one egress rewind, so all N results are live at once right now
        and all N expire on the next evaluation.
        """
        raw = self._require()
        status, values, statuses = raw.exec_many(sources)
        self._last_status = status
        results = [
            _convert(value, raw.read_string(value)) if code == OMNI_OK else None
            for value, code in zip(values, statuses)
        ]
        if status != OMNI_OK and all(code == OMNI_OK for code in statuses):
            # No item failed, so the call itself was refused -- a bad argument or
            # a re-entrant call. There is no per-item shape for that.
            raise OmniError(status, self._message(raw, status))
        return [
            (code, result, raw.batch_error(index) if code != OMNI_OK else "")
            for index, (code, result) in enumerate(zip(statuses, results))
        ]

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

    def register_function(self, name: str, fn: HostFunction, *, arity: int) -> None:
        """Let programs call the Python ``fn`` as ``name(...)``.

        The registration outlives every evaluation: bind it once at start-up and
        call it from as many programs as you like. Defining ``name`` again
        replaces the previous callable, and the engine forgets that one at the
        same moment.

        ``arity`` is required, because a Python signature is not always
        knowable -- ``arity=1`` for :func:`len`, ``arity=2`` for a two-argument
        lambda with the same shape -- and a program that passes a different count
        is rejected by the engine, before your callable is entered, with an error
        naming the function and both counts.

        Args:
            name: at most :data:`MAX_IDENTIFIER_LEN` UTF-8 bytes.
            fn: called with the arguments as ``float`` / ``bool`` / ``str`` /
                ``None``; returns one of those, and anything else becomes a host
                error the program can read.
            arity: 0 to :data:`MAX_ARGS`, the number of arguments a call takes.

        Raises:
            OmniError: the ABI rejected the name, the callable or the arity, or
                this is called from inside a host function of the same engine.
        """
        raw = self._require()
        status = raw.register_host_fn(name, arity, fn)
        self._last_status = status
        if status != OMNI_OK:
            raise OmniError(
                status,
                self._message(raw, status) or f"the ABI rejected the registration {name!r}",
            )

    def unregister_function(self, name: str) -> bool:
        """Drop a registered function; True if the engine had one.

        Idempotent, and the only way to release a callable you no longer need:
        until this returns, the engine holds a pointer into a ctypes trampoline
        this package keeps alive on its behalf.
        """
        return self._require().unregister_host_fn(name)

    @property
    def host_function_count(self) -> int:
        """Registrations this engine currently holds.

        Read from the library rather than from this package's own table, so the
        number is the engine's answer and a registration the SDK thought it had
        but the ABI lost is a difference you can see here.
        """
        return self._require().host_fn_count()

    def stats(self) -> Stats:
        """The engine's memory counters: executions, strings built, bytes held.

        Read them after a benchmark to tell a high-water mark from a leak. The
        arena is bulk-freed per execution and the egress block rewinds, so
        ``*_bytes_reserved`` only ever grows to the largest single request.
        """
        return self._require().stats()

    def close(self) -> None:
        """Release the engine, and every host function registered on it.

        Idempotent. The ctypes closures handed to C are released only after the
        engine has forgotten their addresses, so nothing can call a trampoline
        this package has already let go.
        """
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
        if status == OMNI_ERR_REENTRANT:
            # The ABI's own answer carries no text here: the engine is busy, so
            # the one thing it cannot do is render a message about being busy.
            return (
                "this engine is already inside a host function; a callback may "
                "not call back into the engine that called it"
            )
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
