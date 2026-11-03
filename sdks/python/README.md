# omniengine — Python SDK

OmniEngine from Python, using **only** the standard library's `ctypes`.
`omniengine` installs with **no dependencies at all**, which is the point: an
engine binding that drags in five packages is a harder sell than the engine
itself.

## Install

```bash
pip install -e sdks/python          # from a checkout of this repository
```

The name `omniengine` is what the package metadata will publish under; the
package is **not on PyPI yet**, so today install from the repository path (or
`pip install /path/to/OmniEngine/sdks/python`). A regular, non-editable install
works too, but then the SDK is outside the workspace: run inside the repo, or set
`OMNI_ENGINE_HOME` / `OMNI_ENGINE_LIBRARY` — see below.

The Python package is pure and installs anywhere. The engine is a native
library, and this SDK does **not** bundle a binary — a wheel that ships a
stale `.dll` is a bug nobody notices until production. Build it once:

```bash
cargo build -p omni-ffi
```

## Point the SDK at the library

Tried in order:

| How | What it means |
| --- | --- |
| `OMNI_ENGINE_LIBRARY` | full path to `omni.dll` / `libomni.so` / `libomni.dylib` |
| `OMNI_ENGINE_HOME` | project root; searches `target/debug`, `target/release`, `lib`, `bin` |
| next to the package | a wheel that bundles the artifact (this one does not) |
| upward from this package | a checkout or an editable install just works |
| upward from the current directory | run from anywhere inside the repo |
| the bare file name | a system-wide install found by the OS loader |

Inside this repository you need no environment variables at all.

## Use it

```python
import omni

with omni.Engine() as engine:
    engine.evaluate("let answer = 40 + 2")     # 42.0
    engine.run("20 / 3").value                 # 6.666666666666667
    engine.run("// a comment\n7").tag          # "number"
```

Three ways to handle a bad program, for three kinds of caller:

```python
engine.evaluate("1 / 0")   # raises OmniError(kind="runtime")
engine.run("let = 1")      # raises OmniError(kind="compile")
engine.try_run("###")      # returns (1, None, "line 1: unexpected character '#'")
```

`OmniError.status` is the raw ABI code and `OmniError.message` is the engine's
own text, including the line number — that context survives the C boundary on
purpose.

## What is guaranteed

- Every ABI argument and return type is declared, so a 64-bit handle is never
  truncated to 32 bits.
- `OmniValue`'s size and field offsets are checked at import time against the
  Rust layout; a drift raises `RuntimeError` instead of returning wrong numbers.
- Nothing you get out of an engine is a pointer. The arena is freed at the start
  of every evaluation, so results are copied into Python objects on the way out.
- A Rust panic becomes `OmniError(kind="panic")` rather than a dead process.
- Source longer than the ABI's bound is rejected in Python, before a call.

## Limits

One engine per thread: a handle is not thread-safe, so create one per thread or
serialise the calls. `close()` is idempotent and engines also clean themselves
up at interpreter exit.

Phase 1 evaluates arithmetic. There are no strings yet, so there is nothing to
return besides numbers, booleans, `nil`, and `undefined`.

## Tests

```bash
python sdks/python/tests/test_ffi.py        # 28 tests, stdlib unittest
```

## License

MIT.
