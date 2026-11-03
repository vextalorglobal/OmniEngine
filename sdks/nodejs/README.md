# omniengine — Node.js SDK

OmniEngine from Node, reached through [`koffi`](https://koffi.dev). The JS side
is tiny; what it guarantees is that your handle is a full 64-bit pointer and
your results are plain JavaScript values.

## Install

```bash
npm install ../sdks/nodejs          # a consumer project, installing from this repo
npm install                         # inside sdks/nodejs, to run the SDK's own tests
```

`omniengine` is the name the metadata reserves; the package is **not on npm
yet**, so install from the repository path today. npm symlinks a `file:` install
and Node resolves the real path, so the linked package still sits inside the
workspace and finds the engine with no configuration.

`koffi` is an **optional** dependency. That is deliberate: `npm install` should
not hard-fail on a platform koffi has no prebuild for. Instead you get a clear
error when you construct an `Engine`, and the rest of the package still loads.

```bash
npm install koffi        # if you have not got it; prebuilt, no compiler needed
```

Verified working on Node 26 with koffi 3.3.6, using the prebuild bundled in the
npm tarball — no install script has to run.

## Build the engine once

```bash
cargo build -p omni-ffi
```

The package does not ship a binary. A native artifact hidden inside a JS tarball
goes stale the moment the Rust side changes, and nothing would catch it.

## Point the SDK at the library

Tried in order:

| How | What it means |
| --- | --- |
| `OMNI_ENGINE_LIBRARY` | full path to `omni.dll` / `libomni.so` / `libomni.dylib` |
| `OMNI_ENGINE_HOME` | project root; searches `target/debug`, `target/release`, `lib`, `bin` |
| next to this package | a bundle that places the artifact beside `index.js` |
| upward from this package | a checkout or a linked install |
| upward from the current directory | run from anywhere inside the repo |
| the bare file name, via `PATH` | a system-wide install |

Inside this repository, no environment variables are needed.

## Use it

```js
const { Engine } = require('omniengine');

const engine = new Engine();
engine.evaluate('let answer = 40 + 2');           // 42
engine.run('20 / 3').value;                       // 6.666666666666667
engine.run('"omni" + " " + "engine"').value;      // 'omni engine'
engine.run('"ab" == "a" + "b"').value;            // true
engine.string('"a" + "\\n" + "b"');               // 'a\nb'
```

A `Result` is `{ tag, number, text, value }`. `text` is a `Buffer` with the
exact bytes of a string result — NULs included, because OmniEngine strings are
length-delimited rather than C strings — and `value` is those bytes decoded as
UTF-8. `number` is `0` for a string, on purpose: those eight payload bytes are
an address, and publishing one as a double would be noise in a log.

Give the engine a string of your own — it survives every later call — and then
release the handle:

```js
engine.defineString('greeting', 'hello wörld');
engine.run('greeting + "!"').value;               // 'hello wörld!'
engine.stats().egressBytesReserved;               // 65536: one chunk, reserved
engine.close();                                   // idempotent
```

Every call after `close()` throws `OmniError` with `kind === 'argument'` and the
message `this Engine is closed`. The wrapper refuses rather than answering from a
released handle, which is the same rule in Python: leaving a `with` block closes
the engine.

Or let a block release it:

```js
{
  using engine = new Engine();                    // released at the closing brace
  console.log(engine.evaluate('2 + 3 * 4'));      // 14
}
```

`using` needs a runtime with Explicit Resource Management; this is verified on
Node 26.12. `await using` works too but only where top-level `await` is legal — an
ES module, not the CommonJS file above.

Errors carry the ABI status and the engine's text:

```js
try {
  engine.run('let = 1');
} catch (err) {
  err.kind;    // 'compile'
  err.status;  // 1
  err.message; // "line 1: expected a name, found ''=''"
  String(err); // "compile: line 1: expected a name, found ''=''"
}
```

`err.message` stays exactly what the engine said, matching the Python SDK. Those
doubled quotes are the engine's, not this wrapper's: `display_kind` already
renders a symbol as `'='` and the message template quotes it again. The
one-character fix is recorded in section 10 of `docs/GUIDE.md`; until it lands the
SDKs pass the text through rather than prettify it, because a binding that edits
error messages is a binding that can hide one.

Prefer data to exceptions? `tryRun` returns
`{ status, statusName, result, message }` and never throws.

## What is guaranteed

- The engine handle is a **BigInt**. Converting it with `Number()` would
  truncate any address above 2^53, so the wrapper never does. The one address it
  could read — a string result's — is never converted at all: `omni_str_copy`
  takes the value struct, and the bytes arrive in a `Buffer` you own.
- Source, names and payloads are passed as UTF-8 `Buffer`s with explicit
  lengths, never as NUL-terminated strings the ABI does not expect.
- `OmniValue`, `OmniPayload` and `OmniStats` sizes and every field offset are
  asserted against the C header at load time.
- The library's ABI level is read at load and compared with this package's. A
  stale artifact refuses to import, with `cargo build -p omni-ffi` in the error,
  because a v1 value struct read as v2 gives wrong numbers rather than an error.
- A short copy returns exactly the bytes written. `Buffer.allocUnsafe` is used
  because every byte below the count came from the engine, and when a stale
  length makes the copy shorter, the untouched tail is dropped rather than
  exposed through `.buffer`.
- A Rust panic returns status `3` instead of terminating your process.
- `close()` is idempotent, and the scratch memory koffi allocated is freed.

## Limits

One engine per thread. A Worker needs its own `Engine`, and a handle must never
cross a worker boundary.

Strings are data, not a full string language yet: literals with `\n`, `\r`,
`\t`, `\"`, `\\` and `\u{…}` escapes, `+` concatenation with strict typing
(`"hi" + 42` is a runtime error that names both operands), and `==` / `!=`
content equality. No length, indexing, slicing, methods, interpolation or
ordering comparisons yet — and no `null` or `true` literals, so booleans only
arrive from a comparison.

`text` is a fresh `Buffer` per string result, and that copy is the price of the
rule above. Measure it with `engine.stats()` before optimising around it.

## Tests

```bash
cd sdks/nodejs
npm install koffi
npm test                                   # 56 tests, node:test
```

They need `cargo build -p omni-ffi` — the build, not `cargo test`, because the
tests load the artifact on disk.

## License

MIT.
