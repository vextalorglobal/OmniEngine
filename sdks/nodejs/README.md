# omniengine — Node.js SDK

OmniEngine from Node, reached through [`koffi`](https://koffi.dev). The JS side
is tiny; what it guarantees is that your handle is a full 64-bit pointer and
your results are plain JavaScript values.

## Install

```bash
npm install omniengine koffi         # the release tarball: five platform engines
```

The package is **not on npm yet** (0.1.0 is built and packed, not published), so
today use one of these:

```bash
npm install ../sdks/nodejs          # a consumer project, installing from this repo
npm install                         # inside sdks/nodejs, to run the SDK's own tests
```

npm symlinks a `file:` install and Node resolves the real path, so the linked
package still sits inside the workspace and finds your `target/` build with no
configuration. A **release tarball carries the engine** in
`native/<platform>-<arch>/<lib>` — `win32-x64`, `linux-x64`, `linux-arm64`,
`darwin-x64`, `darwin-arm64` — built from the tagged commit in the same CI run.

`postinstall` loads the artifact for the current machine through the same
`loadBindings` the API uses, so the ABI and the struct layout are checked before
your first script runs, and it never downloads anything. A missing engine is a
warning (the normal state of a checkout install) unless
`OMNI_STRICT_POSTINSTALL=1`; `OMNI_SKIP_POSTINSTALL=1` skips the check.

`koffi` is an **optional** dependency. That is deliberate: `npm install` should
not hard-fail on a platform koffi has no prebuild for. Instead you get a clear
error when you construct an `Engine`, and the rest of the package still loads.

```bash
npm install koffi        # if you have not got it; prebuilt, no compiler needed
```

Verified working on Node 26 with koffi 3.3.6, using the prebuild bundled in the
npm tarball — no install script has to run.

## Build the engine for a checkout

```bash
cargo build -p omni-ffi
```

A checkout install stages nothing; the wrapper finds `target/debug` on its way
up from the package. Only a release tarball carries binaries, and a release
tarball is packed by `prepack` — which refuses a version or ABI that disagrees
with the workspace — so the stale-artifact failure mode is prevented at build
time rather than by refusing to ship. `npm run pack:check` is the strict form:
it also requires every advertised platform's engine to be present.

## Point the SDK at the library

Tried in order:

| How | What it means |
| --- | --- |
| `OMNI_ENGINE_LIBRARY` | full path to `omni.dll` / `libomni.so` / `libomni.dylib` |
| `OMNI_ENGINE_HOME` | project root; searches `target/debug`, `target/release`, `lib`, `bin` |
| `native/<platform>-<arch>/` | the engine inside a release tarball |
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
engine.eval('clamp(12, 0, 10) + min(1, 2) + max(3, 4)');  // 15
engine.eval('upper(trim("  omni  "))');           // 'OMNI'
engine.eval('pow(2, 10) + sqrt(16)');             // 1028
engine.eval('contains("omni engine", "engine")'); // true
engine.eval('len("héllo")');                      // 6 (bytes)
```

A `Result` is `{ tag, number, text, value }`. `eval` skips the wrapper and
returns the language value as a native JS primitive: `number`, `string`,
`boolean`, or `null` for an empty program. Seventeen builtins — `abs`, `min`,
`max`, `clamp`, `floor`, `ceil`, `round`, `pow`, `sqrt`, `len`, `trim`, `upper`,
`lower`, `contains`, `replace`, `slice`, `type_of` — need no registration, and `stats().lastExecMicros` reports the most recent execution
in microseconds. `text` is a `Buffer` with the
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
  err.message; // "[Line 1, Col 5]: expected a name, found ''=''"
  String(err); // "compile: [Line 1, Col 5]: expected a name, found ''=''"
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

## Checking for updates

```js
const { checkUpdates } = require('omniengine');

const result = await checkUpdates();
result.status;      // 'available' | 'up-to-date' | 'abi-mismatch' | 'invalid' | 'error'
result.current;     // '0.1.0'
result.latest;      // '0.2.0'
result.available;   // true when latest > current
result.abi; result.notes; result.asset; result.url;
```

The manifest is fetched over HTTPS (Node 18+ `fetch`; `OMNI_UPDATE_URL`
overrides the default GitHub Releases URL, and `file://` is supported for
testing), parsed for its notes and platform asset, and compared by the engine's
own `omni_check_update`, so the SDK and the CLI cannot disagree about what
"newer" means. It downloads nothing and replaces nothing — `omni update` does
that. The promise never rejects: a failure is `status: 'error'` with the reason
in `.error`, unless you pass `{ raiseOnError: true }`.

To check at construction, opt in:

```js
const engine = new Engine({ updateCheck: true }); // or OMNI_UPDATE_CHECK=1
await engine.updateCheck;                         // UpdateCheck, or null if off
```

The request starts in the background, startup is never blocked, and a failure
stays inside the promise.

## Calling back into JavaScript

A script can call a JS function. Bind it by name, with the arity the script will
use:

```js
engine.registerFunction('shout', (s) => s.toUpperCase() + '!', { arity: 1 });
engine.registerFunction('now', () => 1735689600, { arity: 0 });

engine.string('"say: " + shout("hey")');  // 'say: HEY!'
engine.evaluate('now()');                  // 1735689600
engine.hostFunctionCount;                  // 2
engine.unregisterFunction('now');          // true; the name stops resolving
engine.run('now()');                       // throws: unknown function 'now'
```

Arguments arrive as `number`, `boolean`, `string` or `null`, and the return value
goes back the same way — `undefined` crosses as `nil`, and a number is a
JavaScript `number` because every OmniEngine number is a `double`. Two rules
decide what your function may do:

- **The arguments are borrowed, and they are already decoded.** A string argument
  is a JS `string` by the time your function runs, so there is nothing to copy —
  but keep it only for the duration of the call.
- **Do not call back into the same engine from inside a callback.** The ABI
  refuses it with `OMNI_ERR_REENTRANT` (status 6) rather than handing out a second
  mutable reference, and `lastError()`, `strCopy` and `unregisterFunction` report
  the same refusal as an empty result or `false`. Compute what you need before the
  call, or register a second function.

Anything your function throws is caught at the boundary and becomes
`host function 'shout' failed: Error: …` in the script's error, because an
exception unwinding through the Rust frames that called your callback is undefined
behaviour. A return value the ABI cannot carry — an object, a symbol, a function —
is reported the same way, so a stray `{}` fails loudly instead of crossing as `0`.

The koffi handle and the encoded name are held for as long as the engine knows
them, which is what `close()` releases. Register once at start-up; a callback
re-registered per request pays the encode every time.

## Batching: several programs, one crossing

A loop of `run` pays the FFI crossing and the arena reclamation once per
program; a batch pays both once:

```js
engine.runMany(['1 + 1', '"two"', "shout('hey')"]);
// [ { tag: 'number', value: 2 }, { tag: 'string', value: 'two' },
//   { tag: 'string', value: 'HEY!' } ]
```

Each item is an independent program — item *N* cannot read item *N−1*'s `let` —
and every string result is copied out before the array is returned, so nothing
in it expires. A failing item throws `OmniError` naming its index and its own
message (`item 1 of 3: [Line 1, Col 1]: …`); the items after it still ran. For REPL-style
callers, `tryRunMany` returns one `{ status, statusName, result, message }` per
item instead of throwing — the per-item shape of `tryRun`.

Measured usage guidance: a batch of one is strictly a loss (one extra array is
still extra work), and the win saturates fast — the crossing itself costs ~36 ns
through koffi while one wrapped `run` costs ~8.5 µs, so a batch can only
amortise the smaller term. `node bench.js` prints the numbers for the machine
you are on.

## What is guaranteed

- The engine handle is a **BigInt**. Converting it with `Number()` would
  truncate any address above 2^53, so the wrapper never does. The one address it
  could read — a string result's — is never converted at all: `omni_str_copy`
  takes the value struct, and the bytes arrive in a `Buffer` you own.
- Source, names and payloads are passed as UTF-8 `Buffer`s with explicit
  lengths, never as NUL-terminated strings the ABI does not expect.
- A registration is refused here, before the ABI sees it, when the callable is
  not a function or the arity is not an integer between 0 and `MAX_ARGS` —
  `kind === 'argument'`, and `hostFunctionCount` unchanged. A typo'd callback
  fails where you typed it, not several evaluations later inside a script.
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
- An exception thrown inside a host function becomes status `5` (`host`) with
  your message in the script's error, never an unwind through the Rust frames that
  called your callback.
- A batch of N sources produces N results in order, each one a complete
  `exec`'s worth of state; a failing item does not stop the batch, and the
  reserved egress bytes are bounded by one batch's total demand, not one
  item's. An empty batch is a no-op that expires nothing.
- A batch longer than `MAX_BATCH` (4,096) is refused here, before the ABI
  sees it, with the SDK's own message.
- `close()` is idempotent, and the scratch memory koffi allocated is freed. Every
  registered trampoline is unregistered first, so a live JS closure is never left
  pointing at a dead engine.

## Limits

One engine per thread. A Worker needs its own `Engine`, and a handle must never
cross a worker boundary. A host function registered on an engine is released when
that engine closes, and a callback may not call back into the same engine.

Strings are data, not a full string language yet: literals with `\n`, `\r`,
`\t`, `\"`, `\\` and `\u{…}` escapes, `+` concatenation with strict typing
(`"hi" + 42` is a runtime error that names both operands), and `==` / `!=`
content equality. No length, indexing, slicing, methods, interpolation or
ordering comparisons yet — and no `null` or `true` literals, so booleans only
arrive from a comparison.

A host function takes at most 16 arguments (`MAX_ARGS`) and a program may nest
calls 64 deep (`MAX_CALL_DEPTH`); both are checked by the engine and reported as
errors rather than as a stack overflow. A returned string is capped by the same
`MAX_STRING_LEN` the engine gives a program. One batch crossing carries at most
`MAX_BATCH` (4,096) sources.

`text` is a fresh `Buffer` per string result, and that copy is the price of the
rule above. Measure it with `engine.stats()` before optimising around it.

## Tests

```bash
cd sdks/nodejs
npm install koffi
npm test                                   # 116 tests, node:test
node bench.js                              # measured numbers for this machine
```

They need `cargo build -p omni-ffi` — the build, not `cargo test`, because the
tests load the artifact on disk.

## License

Vextalor Global Public & Integration License (VGPIL) 1.0 — see
[LICENSE](LICENSE). Integration and production use are free; reselling the
source or core binary as a standalone or rebranded product is not permitted.
