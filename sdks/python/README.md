# omniengine — Python SDK

OmniEngine from Python, using **only** the standard library's `ctypes`.
`omniengine` installs with **no dependencies at all**, which is the point: an
engine binding that drags in five packages is a harder sell than the engine
itself.

## Install

```bash
pip install omniengine              # the release wheel: engine included, no build step
```

That name is what the metadata publishes under; **0.1.0 is not on PyPI yet**, so
today there are two local routes:

```bash
pip install -e sdks/python          # from a checkout, using the library you built
cargo build -p omni-ffi

pip install /path/to/omniengine-0.1.0-py3-none-win_amd64.whl   # a wheel you built
```

A **release wheel carries the engine** in `omni/_native/<lib>`, built from the
same tag in the same CI run. A **checkout install carries nothing** and finds
your `target/` build at run time, which is what makes `pip install -e` work with
no configuration.

To build the release wheel yourself, let `setup.py` stage the artifact:

```bash
cargo build --release -p omni-ffi
OMNI_NATIVE=build python -m build --wheel sdks/python
```

`OMNI_NATIVE` selects what goes in: unset/`auto` stages an existing
`target/release/<lib>` or leaves the wheel pure, `build` runs cargo first,
`skip` never stages (editable installs and the test suite), and a path stages
that file. The file must carry this platform's name, load, and answer the ABI
this SDK speaks — a mismatch fails the build. `OMNI_REQUIRE_NATIVE=1` makes
"nothing staged" an error, which is how CI refuses to publish a wheel that
cannot load. The result is a platform-tagged wheel; see
`docs/GUIDE.md` section 6.6 for why CI retags it `py3-none-<platform>`.

## Point the SDK at the library

Tried in order:

| How | What it means |
| --- | --- |
| `OMNI_ENGINE_LIBRARY` | full path to `omni.dll` / `libomni.so` / `libomni.dylib` |
| `OMNI_ENGINE_HOME` | project root; searches `target/debug`, `target/release`, `lib`, `bin` |
| `<package>/_native/<lib>` | the engine inside a release wheel |
| next to the package | a package that ships the artifact beside `__init__.py` |
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

## Batching: several programs, one crossing

A loop of `run` pays the FFI crossing and the arena reclamation once per
program; a batch pays both once:

```python
engine.run_many(["1 + 1", '"two"', "shout('hey')"])
# [Result(tag='number', value=2.0), Result(tag='string', value='two'),
#  Result(tag='string', value='HEY!')]
```

Each item is an independent program — item *N* cannot read item *N−1*'s `let` —
and every string result is copied out before the list is returned, so nothing in
it expires. A failing item raises `OmniError` naming its index and its own
message (`item 1 of 3: line 1: …`); the items after it still ran. For REPL-style
callers, `try_run_many` returns one `(status, result, message)` per item instead
of raising — the per-item shape of `try_run`.

Measured usage guidance: a batch of one is strictly a loss (one extra list is
still extra work), and the win saturates fast — the crossing itself costs ~240 ns
through ctypes while one wrapped `run` costs ~10 µs, so a batch can only amortise
the smaller term. `sdks/python/bench.py` prints the numbers for the machine you
are on.

## Calling back into Python

A script can call a Python function. Bind it by name, with the arity the script
will use:

```python
engine.register_function("shout", lambda s: s.upper() + "!", arity=1)
engine.register_function("now", lambda: 1735689600.0, arity=0)

engine.string('"say: " + shout("hey")')   # 'say: HEY!'
engine.evaluate("now()")                  # 1735689600.0
engine.host_function_count                # 2
engine.unregister_function("now")         # True; the name stops resolving
engine.run("now()")                       # raises: unknown function 'now'
```

Arguments arrive as `float`, `bool`, `str` or `None`, and the return value goes
back the same way. Two rules decide what your function may do:

- **The arguments are borrowed, and they are already decoded.** A string argument
  is a `str` by the time your function runs, so nothing to copy — but keep it only
  for the duration of the call.
- **Do not call back into the same engine from inside a callback.** The ABI
  refuses it with `OMNI_ERR_REENTRANT` (status 6) rather than handing out a second
  mutable reference, and `omni_str_copy`, `omni_last_error` and
  `unregister_function` report the same refusal as `0`. Compute what you need
  before the call, or register a second function.

Anything your function raises — including `BaseException` — is caught at the
boundary and becomes `host function 'shout' failed: ValueError: …` in the
script's error, because an exception unwinding through the Rust frames that called
your callback is undefined behaviour. A return value that is not a number, bool,
`str`, `bytes` or `None` is reported the same way, so a stray dict fails loudly
instead of crossing as `0`.

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
- A Python exception inside a host function becomes `OmniError(kind="host")` with
  the message your script can read, never an unwind through the frames that
  called your callback.
- A batch of N sources produces N results in order, each one a complete `exec`'s
  worth of state; a failing item does not stop the batch, and the reserved egress
  bytes are bounded by one batch's total demand, not one item's. An empty batch
  is a no-op that expires nothing.
- Source, names and payloads longer than the ABI's bounds are rejected in
  Python, before a call — and so is a registration whose `fn` is not callable.
  `register_function` raises `OmniError(kind="argument")` there rather than
  storing a trampoline that would fail as a host error several evaluations later.
- A batch longer than `MAX_BATCH` (4,096) is refused in Python before the
  crossing, with the SDK's own message.

## Limits

One engine per thread: a handle is not thread-safe, so create one per thread or
serialise the calls. `close()` is idempotent and engines also clean themselves
up at interpreter exit. A host function registered on an engine is released when
that engine closes; a callback may not call back into the same engine.

Strings are data, not a full string language yet: literals with `\n`, `\r`,
`\t`, `\"`, `\\` and `\u{…}` escapes, `+` concatenation with strict typing
(`"hi" + 42` is a runtime error that names both operands), and `==` / `!=`
content equality. No length, indexing, slicing, methods, interpolation or
ordering comparisons yet — and no `null` or `true` literals, so booleans only
arrive from a comparison.

A host function takes at most 16 arguments (`MAX_ARGS`) and a program may nest
calls 64 deep (`MAX_CALL_DEPTH`); both are checked by the engine and reported as
errors rather than as stack overflow. A returned string is capped by the same
`MAX_STRING_LEN` the engine gives a program. One batch crossing carries at most
`MAX_BATCH` (4,096) sources.

`Result.text` is a `bytes`, and a `str` you get back is a Python object that
costs one copy. If you are moving megabytes per call, that copy is the price of
the safety rule above; measure it with `engine.stats()` — or run
`sdks/python/bench.py` — before deciding.

## Tests

```bash
python sdks/python/tests/test_ffi.py        # 107 tests, stdlib unittest
python sdks/python/bench.py                 # ns/call, gc/call, batch speed-up
```

They need `cargo build -p omni-ffi` (the build, not `cargo test`: the tests load
the artifact on disk).

## License

Vextalor Global Public & Integration License (VGPIL) 1.0 — see
[LICENSE](LICENSE). Integration and production use are free; reselling the
source or core binary as a standalone or rebranded product is not permitted.
