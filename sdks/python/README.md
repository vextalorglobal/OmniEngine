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
    engine.evaluate("let answer = 40 + 2")          # 42.0
    engine.run("20 / 3").value                      # 6.666666666666667
    engine.run("// a comment\n7").tag               # "number"
    engine.run(r'"omni" + " " + "engine"').value    # 'omni engine'
    engine.run(r'"ab" == "a" + "b"').value          # True
    engine.string(r'"\u{1F600}" + " " + "omni"')    # '😀 omni'
```

A `Result` carries `tag`, `number`, `text` and `value`. `text` is the exact bytes
of a string result — NUL bytes included, because OmniEngine strings are
length-delimited rather than C strings — and `value` is the same bytes decoded as
UTF-8. `number` is `0.0` for a string, on purpose: those eight payload bytes are
an address, and printing one as a float would be noise in a log.

Give the engine a string of your own, and it survives every later call. The next
two blocks continue **inside** the `with` statement above: leaving it calls
`close()`, and a closed engine raises `OmniError(kind="argument")` —
`this Engine is closed` — rather than returning a stale answer.

```python
engine.define_string("greeting", "hello wörld")
engine.run('greeting + "!"').value                  # 'hello wörld!'
engine.stats().egress_bytes_reserved                # 65536: one chunk, reserved
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
- `OmniValue` and `OmniStats` sizes and field offsets are checked at import time
  against the Rust layout; a drift raises `RuntimeError` instead of returning
  wrong numbers.
- The library's ABI level is read at load and compared with the SDK's. A stale
  artifact built from an older commit refuses to import, with the build command
  in the error — which is the only honest response, because a v1 value struct
  read as v2 yields wrong numbers, not an error.
- Nothing you get out of an engine is a pointer. A string result is copied out
  by `omni_str_copy` before it becomes a `str`; the arena is freed and the egress
  block rewound at the start of every evaluation, so an address you held would
  read the *next* execution's bytes rather than crash. That is why this SDK does
  not hand one out.
- `omni_str_copy` checks the address against the engine, so another engine's
  value, a released host binding, or an invented pointer copies zero bytes.
- A Rust panic becomes `OmniError(kind="panic")` rather than a dead process.
- Source, names and payloads longer than the ABI's bounds are rejected in
  Python, before a call.

## Limits

One engine per thread: a handle is not thread-safe, so create one per thread or
serialise the calls. `close()` is idempotent and engines also clean themselves
up at interpreter exit.

Strings are data, not a full string language yet: literals with `\n`, `\r`,
`\t`, `\"`, `\\` and `\u{…}` escapes, `+` concatenation with strict typing
(`"hi" + 42` is a runtime error that names both operands), and `==` / `!=`
content equality. No length, indexing, slicing, methods, interpolation or
ordering comparisons yet — and no `null` or `true` literals, so booleans only
arrive from a comparison.

`Result.text` is a `bytes`, and a `str` you get back is a Python object that
costs one copy. If you are moving megabytes per call, that copy is the price of
the safety rule above; measure it with `engine.stats()` before deciding.

## Tests

```bash
python sdks/python/tests/test_ffi.py        # 63 tests, stdlib unittest
```

They need `cargo build -p omni-ffi` (the build, not `cargo test`: the tests load
the artifact on disk).

## License

MIT.
