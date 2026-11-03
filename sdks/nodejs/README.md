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
engine.evaluate('let answer = 40 + 2');   // 42
engine.run('20 / 3').value;               // 6.666666666666667
engine.close();
```

Or on Node 20.11+ / 22+, where `using` releases the handle for you:

```js
await using engine = new Engine();
console.log(engine.evaluate('2 + 3 * 4'));  // 14
```

Errors carry the ABI status and the engine's text:

```js
try {
  engine.run('let = 1');
} catch (err) {
  err.kind;    // 'compile'
  err.status;  // 1
  err.message; // "line 1: expected a name, found '='"
  String(err); // "compile: line 1: expected a name, found '='"
}
```

`err.message` stays exactly what the engine said, matching the Python SDK.

Prefer data to exceptions? `tryRun` returns
`{ status, statusName, result, message }` and never throws.

## What is guaranteed

- The engine handle is a **BigInt**. Converting it with `Number()` would
  truncate any address above 2^53, so the wrapper never does.
- Source is passed as a UTF-8 `Buffer` with an explicit length, never as a
  NUL-terminated string the ABI does not expect.
- `OmniValue`'s size and offsets are asserted against the C header at load time.
- A Rust panic returns status `3` instead of terminating your process.
- `close()` is idempotent, and the scratch memory koffi allocated is freed.

## Limits

One engine per thread. A Worker needs its own `Engine`, and a handle must never
cross a worker boundary.

Phase 1 evaluates arithmetic, so results are numbers, booleans, `null`, or
`undefined`.

## Tests

```bash
cd sdks/nodejs
npm install koffi
npm test                                   # 26 tests, node:test
```

## License

MIT.
