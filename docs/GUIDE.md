# OmniEngine — Phase 1 initialization and execution guide

Everything below was run on this machine, and every number is measured rather
than estimated. Where the design and the reality diverged, the reality is what
is written here, with the reason.

Original brief: a high-performance custom language and ecosystem in `D:\OmniEngine`,
lightweight and modular, with a Rust workspace, a C ABI that Python and
JavaScript can call, a custom file extension that GitHub recognises, and
SDKs that install through `pip` and `npm`.

---

## 0. Status

| Deliverable | State | Evidence |
| --- | --- | --- |
| Phase 1 — workspace and build policy | done | `Cargo.toml`, `.cargo/config.toml`, `rust-toolchain.toml` |
| Phase 2 — core engine | done | 54 `omni-core` tests |
| Phase 3 — C ABI + header | done | 14 `omni-ffi` tests, PE export table, `examples/c/embed.c` |
| Phase 4 — extension and Linguist | done (tier 1) | `.gitattributes` |
| Phase 5 — SDKs and DX | done | 29 Python + 27 Node tests, CLI, wasm scaffold |
| Same answer from four hosts | done | `scripts/verify-all.sh` |

```
cargo test --workspace        # 78 tests
./scripts/verify-all.sh       # Rust, C, Python, Node all print 42
```

---

## 1. Verified environment

Measured on 2026-11-03.

| | |
| --- | --- |
| rustc | 1.99.1, host `x86_64-pc-windows-gnu` |
| cargo | 1.99.1 |
| gcc | 16.2.0 (MinGW-W64 x86_64-ucrt-posix-seh, WinLibs) |
| Python | 3.15.0 |
| Node / npm | 26.12.0 / 11.20.0 |
| **C:** | 100 GB total, **7.5 GB free (93 % used)** |
| **D:** | 138 GB total, **75 GB free (46 % used)** |
| `CARGO_HOME` | 982 MB, of which 970 MB is the registry — all of it from other projects |
| `D:\OmniEngine\target` | 196 MB (114 MB `deps`, 56 MB `incremental`) |
| `target/debug/omni.dll` | 5.2 MB |
| Rust sources | 2,719 lines (`crates/**/*.rs`) |
| Python, JavaScript, and C sources | 1,840 lines (`sdks/`, `examples/c/`) |

**The storage premise needed correcting.** The brief said 20 GB allocated, and
the natural reading was "keep it small on D:". D: has 75 GB free and is not the
problem. C: — where `CARGO_HOME`, the toolchain, and every default build cache
live — has 7.5 GB free. That is the constraint that shapes the build policy,
and it is why section 2.4 exists.

---

## 2. Phase 1 — Environment and workspace setup

### 2.1 Create it

The repository already exists at `D:\OmniEngine`. On a fresh machine:

```bash
# 1. Toolchain. The workspace pins itself to GNU (see 2.6), so install that.
rustup toolchain install stable-x86_64-pc-windows-gnu
rustup component add rustfmt clippy --toolchain stable-x86_64-pc-windows-gnu

# 2. Clone or copy the workspace
git clone <your-remote> /d/OmniEngine
cd /d/OmniEngine

# 3. First build — rustup honours rust-toolchain.toml, so no flag is needed
cargo test --workspace
cargo build -p omni-ffi

# 4. Prove it works end to end
./scripts/verify-all.sh
```

### 2.2 Layout

```
OmniEngine/
├── Cargo.toml                 workspace + build profiles
├── rust-toolchain.toml        pinned: stable-x86_64-pc-windows-gnu
├── .cargo/config.toml         aliases and shared flags, checked in
├── .gitattributes             EOL policy and Linguist rules (Phase 4)
├── crates/
│   ├── omni-core/             value.rs arena.rs lexer.rs engine.rs   (no deps)
│   ├── omni-ffi/              src/lib.rs, include/omni.h, cbindgen.toml, tests/
│   ├── omni-cli/              src/main.rs                            (no deps)
│   └── omni-wasm/             scaffold, excluded from default builds
├── sdks/
│   ├── python/                omni/{__init__.py,_ffi.py}, tests/, pyproject.toml
│   └── nodejs/                index.js, lib/{bindings,discover}.js, tests/
├── examples/                  hello.omni, arithmetic.omni, c/embed.c
├── scripts/verify-all.sh
└── docs/                      this guide, specs/, plans/
```

### 2.3 Why the workspace is shaped this way

```toml
[workspace]
resolver = "3"
members = ["crates/omni-core", "crates/omni-ffi", "crates/omni-cli", "crates/omni-wasm"]
default-members = ["crates/omni-core", "crates/omni-ffi", "crates/omni-cli"]
edition = "2024"
```

Four things, each with a reason:

- **`omni-core` has zero dependencies.** Not minimalism for its own sake: the
  audit surface stays empty, the compile graph stays small, and — the part that
  matters on this machine — `CARGO_HOME` never grows by a byte for this project.
- **`omni-wasm` is a member but not a default member.** `cargo build` then never
  asks for the `wasm32-unknown-unknown` target, which is not installed here. A
  wasm build is an explicit `cargo build -p omni-wasm --target wasm32-unknown-unknown`.
- **`omni-ffi` is the only crate allowed to name `#[repr(C)]` or `extern "C"`.**
  `omni-cli` and `omni-wasm` consume `omni-core` directly, so the core cannot
  quietly become FFI-shaped to satisfy one caller.
- **One `target/` for the whole workspace.** Split target directories multiply
  the disk cost and defeat incremental reuse between crates. It stays on D:,
  where the space is; putting it on C: would be exactly wrong.

### 2.4 Build policy: the part that protects the drive

```toml
[profile.dev]
opt-level = 0
debug = "line-tables-only"    # the single highest-value knob here
incremental = true
codegen-units = 256
panic = "unwind"              # load-bearing, see below

[profile.release]
opt-level = 3
lto = "thin"
codegen-units = 1
panic = "unwind"
strip = "debuginfo"
```

`debug = "full"` on Windows makes MSVC Program Databases that run to multiple
gigabytes for a workspace this size, and they land on the drive with 7.5 GB
free. `line-tables-only` keeps file and line in backtraces — which is what an
engine under development actually needs — without the variable and type
metadata that costs the gigabytes.

`panic = "unwind"` is **not** a preference. `omni-ffi` converts panics to
`OMNI_ERR_PANIC` with `catch_unwind`, which is what stops a Rust panic from
unwinding into a Python or Node process and being undefined behaviour. Setting
`panic = "abort"` in either profile would silently disable that guarantee, so
it is called out in the manifest comment.

`sccache` was deliberately **not** installed. It helps on large dependency
graphs; with zero dependencies, its benefit here is small and it would add
another cache directory to C:.

### 2.5 What is *not* configured, and why

`CARGO_HOME` stays at `C:\Users\m7004\.cargo`. That was an explicit instruction,
and it is honoured: `.cargo/config.toml` does not set it, `build.target-dir` is
not set either, and nothing in this workspace relocates anything.

If that ever changes, the opt-in is two lines and the tradeoff is that your
registry cache and installed binaries move with it:

```bash
# Optional, NOT applied here. Frees ~982 MB from C: after a re-download.
setx CARGO_HOME "D:\cargo-home"
# then move or re-fetch, and restart the shell
```

### 2.6 The toolchain: MSVC did not work, so GNU is pinned

The obvious choice on Windows is MSVC. It is installed here — Visual Studio's
linker exists at

```
D:\VS\VC\Tools\MSVC\14.51.36231\bin\Hostx64\x64\link.exe
```

and the Windows SDK is at `D:\Windows Kits\10\`. But `cargo build` failed with:

```
link: missing operand after '■' / Try 'link --help' for more information.
```

That is not a Rust error. `where link` resolves to

```
C:\Users\m7004\.qoder\bin\git\usr\bin\link.exe
```

— the coreutils `link` from Git for Windows, which shadows MSVC's linker and
takes completely different arguments. rustc therefore invoked the wrong program.

Two fixes were available: put the MSVC directory ahead of Git's `usr\bin` on
`PATH` (or always build from a Developer Command Prompt), or pin the workspace to
the GNU toolchain, whose `ld` is unambiguous and already on PATH. The first
changes the machine; the second changes this project only. Since nothing here
authorised machine-wide `PATH` surgery, **the workspace pins GNU** in
`rust-toolchain.toml`, with this whole story in that file's comments.

To use MSVC instead, from a Developer Command Prompt:

```bash
rustup toolchain install stable-x86_64-pc-windows-msvc
cargo +stable-x86_64-pc-windows-msvc test --workspace
```

Nothing in the workspace depends on the ABI: no C compiler is invoked by the
build, no build script runs, and the only `unsafe` is inside `omni-core` and
`omni-ffi`. What *does* differ by toolchain is the artifact naming — GNU emits
`libomni.a` and `libomni.dll.a`, MSVC would emit `omni.lib` and `omni.dll.lib` —
so the SDKs look for `omni.dll`, which both produce. The MSVC path above is
documented, not tested: this machine has no MSVC Rust toolchain installed, and
installing one to check would cost several hundred megabytes on the drive this
section is trying to protect.

---

## 3. Phase 2 — Core engine initialization

`crates/omni-core` is the language runtime. It has no dependencies, and its
`unsafe` is confined to one file.

### 3.1 Values are eight bytes, with no garbage collector

The brief said "memory-safe data processing without garbage collection
bottlenecks". That is a data-layout decision before it is an algorithm one, so
`Value` is NaN-boxed:

```rust
#[repr(transparent)]
pub struct Value(u64);
```

A double is already a 64-bit pattern, and the IEEE-754 exponent-all-ones
mantissa-non-zero row is almost never produced by arithmetic. OmniEngine
spends two of those rows:

| Bits 63..48 | Meaning | Payload |
| --- | --- | --- |
| anything else | an `f64` | the number itself |
| `0x7FF7` | **pointer** | 48-bit arena address |
| `0x7FF8` + low bits `1..=15` | **singleton** | `nil`, `true`, `false`, `undefined` |
| `0x7FF8` + low bits `0` | an actual `NaN` | no payload |

`0x7FF7_xxxx` is the *signaling* NaN row, which no IEEE arithmetic is permitted
to produce, so a computed number can never be mistaken for a heap address.
`0x7FF8_0000_0000_000n` is the quiet NaN row; slot 0 stays the canonical `NaN`
so `from_f64(f64::NAN)` round-trips as a number.

This came out of a design review, not luck. The first draft tagged pointers as
`v >> 48 == 0`, which is wrong in a way that would have been catastrophic and
silent: `0.0` *is* `0x0000_0000_0000_0000`, so every zero — and every denormal
below about 2^-969 — would have been read as a wild pointer. The regression test
`zero_is_never_a_pointer` exists to keep that from ever coming back.

`Value` is `Copy`, is exactly 8 bytes (asserted in a test), and comparisons
follow IEEE semantics rather than Rust's: `NaN != NaN`, and `-0.0 == 0.0`.

### 3.2 Allocation is a chunked bump arena

```rust
pub struct Arena { /* chunks, bump pointer, stats */ }
```

- Chunks start at 64 KiB (`DEFAULT_CHUNK`), grow geometrically, and never shrink
  below 4 KiB (`MIN_CHUNK`).
- Allocation is a pointer bump. There is no per-object bookkeeping, so
  allocation costs are a compare and an add.
- Each chunk stores the `Layout` it was allocated with, and frees itself with
  that same layout. Reconstructing a `Box<[u8]>` from a 16-byte-aligned
  allocation would deallocate with alignment 1, which is undefined behaviour —
  this is the classic bump-arena mistake and it is why `Chunk` holds its layout.
- **`reset()` frees everything at once**, in O(chunks). This is the "no GC"
  story: nothing is traced, so there is no pause and no collector to tune.

Over that raw arena sits a safe facade:

```rust
pub struct Slot<T> { /* NonNull<T> */ }
impl<T: Copy> Slot<T> {
    pub fn new(arena: &mut Arena, value: T) -> Slot<T>;
    pub fn get(&self) -> T;
    pub fn set(&mut self, value: T);
}
```

Callers work with `Slot<Binding>`, never with `*mut u8`. That is what makes the
"unsafe is confined" claim checkable: only `arena.rs` dereferences raw pointers
inside the core.

The invariant the facade depends on, and that the engine upholds: **every
collection of `Slot`s must be dropped before the arena is reset.**
`begin_execution()` resets the arena and clears the bindings vector in the same
call, so the invariant cannot be broken by a caller.

### 3.3 The lexer borrows; it does not allocate

`Token<'a>` holds `&'a str` slices of the source. Lexing a program allocates
nothing for identifiers or numbers, which is verified by a test comparing
pointer addresses (`token_text_is_a_borrow_not_a_copy`), not by a comment
claiming it.

Numbers accept `42`, `3.5`, `1e3`, `1.5e-3`; they reject `1.` and `1e+` with a
message carrying the line and column. Comments are `//` to end of line — and the
`/` that starts a comment is the same character as division, which is why
`20 / 3` needed its own lexer arm and its own regression test
(`division_is_an_operator_not_a_comment`).

A byte-walking lexer over UTF-8 has one obvious risk: skipping to the next
newline could split a multi-byte character, and a comment in another script would
be the first thing to hit it. `// নম্বর\n42` therefore evaluates to 42 in all four
hosts — `non_ascii_comments_lex_and_keep_line_count` and
`a_program_with_a_non_ascii_comment_evaluates` on the Rust side, and equivalent
assertions in both SDK suites. Identifiers stay ASCII (`[A-Za-z_][A-Za-z0-9_]*`)
by decision rather than accident, so a non-ASCII name is a lex error that *names
the character* — `unexpected character 'উ'` — instead of silently becoming an
undefined variable. Columns are byte offsets from the start of the line, which is
what makes the lexer cheap; the error text carries the decoded character so the
number is never the only clue.

### 3.4 The engine parses and evaluates in one pass

Phase 1 deliberately has **no AST type**. `Engine::exec` lexes once and then
walks the token stream while evaluating, because an expression language with no
functions has nothing to reuse a tree for. Adding an AST later is a
refactor-with-a-home in Phase 2, not a rewrite.

```
program := ( stmt )*                 // ';' is an optional separator
stmt    := 'let' IDENT '=' expr | expr
expr    := term (( '+' | '-' ) term)*
term    := factor (( '*' | '/' | '%' ) factor)*
factor  := ( '-' | '+' )? primary
primary := NUMBER | IDENT | '(' expr ')'
```

Precedence comes from the recursive descent, left-to-right, so `2 + 3 * 4` is
14 and `(2 + 3) * 4` is 20 — both tested.

Semantics worth knowing, because they are the ones that were chosen rather than
fallen into:

- `let` bindings live for **one** execution; the arena and the binding slots are
  reset together on the next `exec`. Host-side names installed with
  `Engine::define` persist.
- Script `let` shadows a host binding of the same name, and script bindings are
  never visible to the host.
- `1 / 0` is `Runtime` ("division by zero") and `1 % 0` is `Runtime`
  ("remainder by zero") — data, not panics, not IEEE infinities leaking outward.
- An empty program evaluates to `undefined`, successfully.
- Identifiers are capped at 32 bytes (`MAX_IDENTIFIER_LEN`) because a binding
  name is stored inline in a fixed `[u8; 32]` array — a limit that exists so the
  arena copy is a single bump, and that returns a clean parse error rather than
  truncating silently.
- Every error carries a line number, and that number survives the FFI boundary
  (section 4.4).

### 3.5 What Phase 1 does not have

No strings, booleans as literals, `nil`/`undefined` literals, functions, control
flow, or collections. The memory model that makes those cheap is in place; the
syntax is not. `Engine::exec` is the only entry point, and it takes `&str`.

---

## 4. Phase 3 — Universal bridge and FFI layer

### 4.1 What a build produces

```bash
cargo build -p omni-ffi
```

Verified output in `target/debug/` on this machine:

| File | Purpose |
| --- | --- |
| `omni.dll` (5.2 MB) | what Python, Node, and `LoadLibrary` load |
| `libomni.dll.a` | MinGW import library, for C link time |
| `libomni.a` | static archive |
| `libomni.rlib` | lets the Rust contract tests call the same surface |

The library is named `omni`, not `omni_ffi`, via `[lib] name = "omni"` — so the
artifact a host loads matches `omni.h` and what the SDKs look for. That was a
correction: the manifest originally promised `omni.dll` and produced
`omni_ffi.dll`.

The exports are not assumed. `objdump -p target/debug/omni.dll` shows exactly
seven, and that is the check:

```
omni_debug_panic  omni_engine_free  omni_engine_new  omni_exec
omni_exec_number  omni_last_error   omni_version
```

### 4.2 The two rules that make this sound

**No arena pointer ever crosses the boundary.** The arena is bulk-freed at the
start of every `exec`, so a `Value` pointing into it would dangle on the next
call. Results therefore leave as a 16-byte `{ tag, number }` struct passed by
value, and text leaves by being **copied into storage the caller owns**.

**A Rust panic may never unwind into a host.** Unwinding past `extern "C"` is
undefined behaviour, and it is the most common defect in hand-rolled Rust FFI.
Every body runs inside `catch_unwind(AssertUnwindSafe(..))` and a caught panic
becomes `OMNI_ERR_PANIC`. There is a test for it, driven from both sides:
`omni_debug_panic` returns 3, and the *same* engine then evaluates `1 + 1` to 2.

### 4.3 The surface

| Function | Returns |
| --- | --- |
| `omni_version()` | static NUL-terminated string, never freed |
| `omni_engine_new()` | opaque `OmniEngine*`, null on failure |
| `omni_engine_free(e)` | nothing; null is a no-op |
| `omni_exec(e, src, len, out)` | status; writes `OmniValue` |
| `omni_exec_number(e, src, len, out)` | status; writes a `double` |
| `omni_last_error(e, buf, cap)` | bytes written, excluding the NUL |
| `omni_debug_panic(e)` | status — a test hook, not an API |

Statuses: `0 OK`, `1 ERR_COMPILE`, `2 ERR_RUNTIME`, `3 ERR_PANIC`,
`4 ERR_ARG`. Tags: `0 NUMBER`, `1 NIL`, `2 BOOL`, `3 UNDEF`.

The layout is pinned on both sides so a drifting SDK fails at compile time
rather than returning wrong numbers at run time:

```rust
const _: () = assert!(std::mem::size_of::<OmniValue>() == 16);
const _: () = assert!(std::mem::offset_of!(OmniValue, number) == 8);
```

with `_Static_assert` in `omni.h`, a `ctypes.sizeof` check at Python import, and
a `koffi.sizeof` check at Node load.

`OmniValue` is a struct, not a union. The spec described a union; a union's
only arm today would be a pointer that nothing populates, and every host would
still have to read it unsafely. It becomes a union when strings arrive.

### 4.4 Two ABI details that only showed up under testing

**`omni_last_error` copies the rendered error, not just the message.** The
first version copied `error.message`, which meant a host received
`expected a name` and had no way to tell the user which line. It now copies the
`Display` form — `line 1: expected a name, found '='` — and the contract tests
assert the location survives.

**A zero-length program is legal even from a null pointer.** Node hands an empty
`Buffer` to C as NULL, and `slice::from_raw_parts(null, 0)` is undefined
behaviour even though it reads nothing. So `omni_exec(e, NULL, 0, &out)` now
short-circuits to the empty program and returns `UNDEF`, while
`omni_exec(e, NULL, 5, &out)` is still `ERR_ARG`.

### 4.5 The header, and where it came from

`crates/omni-ffi/include/omni.h` is hand-written, because cbindgen cannot emit
the `OMNI_API` / `OMNI_CALL` macros, the ABI asserts, or the caller contract.
`cbindgen.toml` is configured so regeneration is one reviewed command rather
than a guess:

```bash
cargo install cbindgen   # once
cbindgen --config crates/omni-ffi/cbindgen.toml crates/omni-ffi \
         --output crates/omni-ffi/include/omni.h
```

`examples/c/embed.c` is the proof. It compiles warning-free under
`-std=c11 -Wall -Wextra` and links against the import library:

```bash
gcc -std=c11 -Wall -Wextra -I crates/omni-ffi/include \
    examples/c/embed.c target/debug/libomni.dll.a -o embed
./embed     # 42, precedence, division, errors, panic boundary, all pass
```

### 4.6 Concurrency

One handle, one thread. `&mut Engine` is what the ABI assumes internally, and
the SDKs document the rule instead of pretending to enforce it. `Send` is
implemented for the arena types because the handle moves cleanly between
threads — it is just not shared.

---

## 5. Phase 4 — GitHub Linguist and file extensions

### 5.1 Extensions

`.omni` is the primary extension; `.ox` is the short alias. Both are declared in
`.gitattributes` with `eol=lf`, and both are used by `examples/`.

### 5.2 What `.gitattributes` does

The rules that matter, quoted from `.gitattributes` (comments abridged):

```gitattributes
* text=auto eol=lf

*.rs    text eol=lf
*.omni  text eol=lf
*.ox    text eol=lf

*.dll  binary
*.so   binary
*.dylib binary
*.wasm binary

*.omni linguist-language=Rust linguist-detectable=true
*.ox   linguist-language=Rust linguist-detectable=true
/sdks/** linguist-vendored=true
/docs/** linguist-documentation=true
crates/omni-ffi/include/omni.h linguist-generated=false
```

The binary rules are not ceremony: a CRLF-converted `omni.dll` simply stops
loading, with an error that hints at nothing.

Checked, not assumed:

```
$ git check-attr -a examples/hello.omni
examples/hello.omni: text: set
examples/hello.omni: eol: lf
examples/hello.omni: linguist-language: Rust
examples/hello.omni: linguist-detectable: true

$ git check-attr linguist-vendored -- sdks/python/omni/__init__.py
sdks/python/omni/__init__.py: linguist-vendored: true

$ git check-attr linguist-documentation -- docs/GUIDE.md
docs/GUIDE.md: linguist-documentation: true
```

Ordering matters in gitattributes — the last matching rule wins for a given
attribute — which is why the path-scoped Linguist rules sit at the end of the
file: a narrower exemption, such as "but keep `sdks/nodejs/tests` counted", only
takes effect if it comes *after* `/sdks/** linguist-vendored`. The extension and
path rules above it set different attributes, so they coexist rather than fight.

Two separate jobs, deliberately kept apart:

**EOL normalization** — one line-ending policy for the repository, so a Windows
checkout and a Linux checkout do not fight over `Cargo.lock`.

**Language statistics** — `linguist-vendored` removes `sdks/**` from the counts,
and `linguist-documentation` removes `docs/**`. That is what makes the brief's
"without altering native project statistics" true *in this repository*: the
1,840 lines of Python, JavaScript, and C in the SDKs and examples do not dilute
the Rust.

### 5.3 The honest limit, and the two-tier plan

The brief assumed `.gitattributes` could make GitHub display a chosen language
name. It cannot. Linguist only maps extensions onto languages it already knows;
`linguist-language=Anything` is validated against its own grammar list, and an
unknown value is ignored. There is no supported per-repository custom language.

So the plan has two tiers, and only the first one is a configuration file:

**Tier 1 — works today, committed.** `.omni` and `.ox` files are attributed to
Rust, which is true at this stage: an `.omni` file in Phase 1 contains an
expression language whose implementation is the repository's Rust. The
repository's language bar shows Rust, and the SDK directories are excluded.

**Tier 2 — needs an upstream PR.** For "Omni" to be its own colour and name
across GitHub, `github/linguist` must learn it. That means, in one pull request:
a `lib/linguist/languages.yml` entry with a real `color` and both extensions, a
grammar under `vendor/` (Linguist will not accept a language with no lexer,
because it measures comment and keyword syntax), tests, and a sample file. Then
GitHub deploys the new Linguist, and the repository needs no change beyond
dropping `linguist-language=Rust` from those two lines.

Until that merges, any claim that GitHub shows "Omni" would be false, so it is
not made anywhere in this repository. `scripts/` and `crates/` are left in the
statistics on purpose — they are the engine.

---

## 6. Phase 5 — Developer experience and the SDKs

Both SDKs are thin, both are stdlib-only or nearly so, and neither vendors the
engine binary. A consumer installs the wrapper and points it at a library that
already exists on their machine.

### 6.1 Python

```bash
# Today, from a checkout:
python -m venv .venv && source .venv/Scripts/activate   # bash on Windows
pip install -e sdks/python

# The registry name the metadata reserves, once Phase 2 publishes it:
pip install omniengine
```

Neither package is on PyPI or npm yet. `pip install -e` and `npm install <path>`
are what was actually run and verified here; the registry form is the intent,
recorded in `pyproject.toml` and `package.json`, not a claim about the present.

```python
from omni import Engine

with Engine() as engine:
    print(engine.evaluate("let answer = 40 + 2"))   # 42.0
    result = engine.run("2 + 3 * 4")
    print(result.tag, result.value)                  # number 14.0
```

`run` returns a frozen `Result` (`tag`, `number`, `value`); `evaluate` returns a
`float` and raises `OmniError` on any non-number or failure; `try_run` returns
`(status, result | None, message)` and never raises. `close()` is idempotent and
`__del__` is guarded by an `atexit` hook, so an engine that outlives the script
is not a use-after-free.

The dependency list in `pyproject.toml` is empty and that is load-bearing: the
binding is `ctypes`. `cffi`, `pybind11`, and a build-time compiler would each add
an install failure mode to a package whose entire job is to find one DLL.

License metadata uses PEP 639 (`license = "MIT"` plus `license-files`), which
needs `setuptools>=77`; the table-form `license = {text = "MIT"}` emits a
deprecation warning on current setuptools.

### 6.2 Node.js

```bash
npm install ../sdks/nodejs        # a consumer project installing from this repo
npm install                       # inside sdks/nodejs, for the SDK's own tests
```

```js
const { Engine } = require('omniengine');

const engine = new Engine();
console.log(engine.evaluate('let answer = 40 + 2'));   // 42
console.log(engine.run('2 + 3 * 4').value);            // 14
engine.close();
```

`run` → `Result`; `evaluate` → number; `tryRun` → `{ status, result, message }`
with no throw. With Node 26 and `--experimental-strip-types`-era tooling,
`Symbol.dispose` is wired on the prototype so `using engine = new Engine()` frees
the handle deterministically; it is defined conditionally because the symbol does
not exist on older runtimes.

`koffi` is an **optionalDependency** and `dependencies` is empty. If koffi has no
prebuild for a platform, `npm install` still succeeds and the failure surfaces as
an `OmniLibraryError` with an actionable message when you construct an `Engine`,
rather than as a broken install. koffi 3.3.6 was verified working on Node 26 with
npm's install script blocked, which is the common hardened-CI configuration.

The alternative, `ffi-napi`, was rejected: it is unmaintained and breaks on
modern Node. `node-gyp` was rejected because it makes every install a compile.

Two things the Node binding had to get right, both found by testing:

- Handles are **BigInt**. koffi returns a pointer as a 64-bit integer, and a
  plain JS number would silently truncate it on a 64-bit address space.
- An empty string becomes a NULL pointer plus zero length on the way in, which
  is why the ABI handles it (section 4.4) rather than the wrapper working around
  it.

### 6.3 How a wrapper finds the library

Identical order in both SDKs, most explicit first:

| Priority | Python | Node |
| --- | --- | --- |
| 1 | `OMNI_ENGINE_LIBRARY` (full path) | `OMNI_ENGINE_LIBRARY` |
| 2 | `OMNI_ENGINE_HOME` + `target/{debug,release}`, `lib`, `bin` | same |
| 3 | next to the package (a bundled wheel) | next to `index.js` |
| 4 | upward from the package, `target/{debug,release}` at 6 levels | same |
| 5 | upward from the current working directory | same |

The upward walks exist because an installed wheel sits in `site-packages`,
nowhere near `D:\OmniEngine\target\debug`. The first version of the discovery
code resolved the package's own location and found the temporary directory the
venv happened to live in — `…\Temp\omnivenv\target\debug` — which was correct
code answering the wrong question. Walking from the package *and* from the
working directory is what makes an editable install, a linked install, and a run
from inside the repo all work with no configuration.

One subtlety worth keeping: the walk checks the starting directory *before*
ascending. A version that ascended first could not find `<repo>/target/debug`
when run from the repository root, which is the most common case of all. There
are tests for both of these, in Python and in Node.

Measured, with the engine built and no environment variables set:

| How it was installed | Where the script runs | Result |
| --- | --- | --- |
| `pip install -e sdks/python` | anywhere | found — the package lives in the repo, so the walk reaches `target/` |
| `pip install sdks/python` (a real wheel) | inside the repo | found — the working-directory walk reaches `target/` |
| `pip install sdks/python` | outside the repo | **not found**, by design; set `OMNI_ENGINE_HOME` or `OMNI_ENGINE_LIBRARY` |
| `npm install ../sdks/nodejs` | anywhere | found — npm symlinks `file:` installs, Node resolves the real path, so the package is in the repo |
| both | anywhere, `OMNI_ENGINE_HOME=D:\OmniEngine` | found |

If nothing is found, the error prints the build command, the list of paths it
tried, and the loader's own message. That is the whole failure mode, and it is
deliberate: a wheel that silently shipped its own stale `.dll` would be a worse
bug, because nobody would notice until production.

### 6.4 The CLI

```bash
cargo run -p omni-cli -- run examples/hello.omni
cargo run -p omni-cli -- eval "2 + 3 * 4"
```

Exit codes are 0 success, 1 program error, 2 usage or I/O error, so a shell
script can tell "your expression is wrong" from "you invoked me wrong". Argument
parsing is hand-rolled; `clap` would add a dozen crates and a few hundred MB of
build cache for three subcommands.

### 6.5 The wasm scaffold

`crates/omni-wasm` is a workspace member but not a default member, so
`cargo build` never needs `wasm32-unknown-unknown`, which is not installed here.
It mirrors the C ABI shape — number in, tagged result out, errors as strings,
and a `status_code` map asserted to agree with the C integers — so that adding
`#[wasm_bindgen]` in Phase 2 is a decorator pass over a surface that is already
tested, not a redesign.

```bash
cargo check -p omni-wasm                                        # works today
cargo build --target wasm32-unknown-unknown -p omni-wasm        # needs the target
```

`wasm-bindgen` is deliberately absent from `Cargo.toml`. Declaring a dependency
that cannot compile on this machine would put an unusable entry in `Cargo.lock`.

---

## 7. Verification

One command, and it is the acceptance test for the whole brief:

```bash
./scripts/verify-all.sh
```

Measured output on 2026-11-03:

```
== OmniEngine cross-host verification ==
   workspace: /d/OmniEngine

Rust tests... Shared library... omni-cli... C embedding... Python SDK... Node SDK... 
== Results ==
  PASS  cargo test --workspace (78 tests)
  PASS  omni.dll (5.2M)
  PASS  omni run examples/hello.omni -> 42
  PASS  examples/c/embed.c -> 42 via omni.h
  PASS  python ctypes -> 42.0
  PASS  python SDK tests
  PASS  node koffi -> 42
  PASS  node SDK tests (ℹ tests 27 ℹ pass 27 ℹ fail 0 )

All checks that could run, passed.
```

The script degrades to `SKIP` rather than lying: the wasm row is skipped because
the target is not installed, and a missing host is reported as skipped, not as a
pass.

Against the brief's five phases:

| Phase | Asked for | Verified by |
| --- | --- | --- |
| 1 | Workspace on D:, fast compiles, managed `target/` | `cargo test --workspace` 78 tests; `target/` measured at 175 MB |
| 2 | Ultra-fast memory-safe core, no GC | NaN-boxed 8-byte `Value` asserted for size; arena reset is O(chunks); no collector exists to pause |
| 3 | C ABI callable from Python and JavaScript | same `42` from Rust, C, Python, Node; 7 exports confirmed with `objdump -p`; `omni.h` compiles warning-free under `-Wall -Wextra` |
| 4 | A custom extension GitHub recognises | `.gitattributes` tier 1; the tier-2 limit is stated in section 5.3 rather than glossed over |
| 5 | `pip`/`npm` SDKs that do not move the language stats | 29 + 27 SDK tests; `linguist-vendored` on `sdks/**`; no binary vendored into either package |

Test totals: **54** in `omni-core`, **14** in `omni-ffi` (4 unit plus 10 in
`tests/ffi_contract.rs`, which drive the ABI exactly as a host does), **6** in
`omni-cli`, **4** in `omni-wasm`, **29** Python, **27** Node.

Static checks are part of the definition of done:

```bash
cargo clippy --workspace --all-targets -- -D warnings   # clean
cargo fmt --all --check                                  # clean
```

---

## 8. Storage ledger

Measured, all of it on this machine, at the time of writing:

| What | Size | Where | Note |
| --- | --- | --- | --- |
| Source tree (excluding `target/`) | 3.4 MB | D: | the whole repository |
| `target/` total | **175 MB** | D: | `deps` 114 MB, `incremental` 58 MB |
| `target/debug/omni.dll` | 5.2 MB | D: | the artifact every host loads |
| `CARGO_HOME` | 982 MB | C: | 970 MB registry, none of it from this project |
| `sdks/nodejs/node_modules` | 2.9 MB | D: | koffi prebuild, gitignored |
| Free on C: | **7.5 GB** (93 % used) | | the binding constraint |
| Free on D: | 75 GB (46 % used) | | |

`cargo clean` returns the 175 MB in a second, and it is the only routine way to
do so. The knobs that keep it that size are in section 2.4: no dependencies, so
no dependency artifacts; `debug = "line-tables-only"`, so no PDBs.

**Zero dependencies is a storage decision, not an aesthetic one.** `omni-core`
and `omni-cli` have no third-party crates, so building and testing this project
fetched nothing: the only file under `CARGO_HOME` — on the drive with 7.5 GB free
— touched during the whole Phase 1 build is cargo's own `.global-cache` metadata,
and the 982 MB total is unchanged. Adding `clap`, `cffi`, or `wasm-bindgen` is a
disk-cost decision here, not just a complexity one.

`CARGO_HOME` relocation was **not** applied, per instruction. It stays at
`C:\Users\m7004\.cargo`; section 2.5 documents the opt-in instead.

---

## 9. Troubleshooting

**`link: missing operand after '■'`** — MSVC's linker is being shadowed by Git
for Windows' coreutils `link.exe`. Run `where link`. This workspace pins the GNU
toolchain to avoid the problem entirely; section 2.6 has the full story.

**The SDKs load an old engine.** `cargo test -p omni-ffi` rebuilds the *test*
binary, not necessarily `omni.dll` — and the SDKs and `embed.c` load `omni.dll`.
After changing the ABI, run `cargo build -p omni-ffi` explicitly, then re-verify.
This bit me once, and it is why `verify-all.sh` builds the library before it
tests the hosts.

**`omni.OmniLibraryError: could not find omni.dll`** (an `OSError` subclass, and
Node's `OmniLibraryError` behaves the same way) — the engine was never built, or
you are running outside the workspace with a plain wheel install.
`cargo build -p omni-ffi`, or set `OMNI_ENGINE_LIBRARY`. The message lists the
paths it tried, so you can tell "not built" from "built somewhere unexpected".

**`npm install` fails on koffi's install script** — expected in a locked-down CI,
and not fatal: koffi ships prebuilds and is an optional dependency. `node -e
"require('koffi')"` confirms it loaded.

**`BackendUnavailable: Cannot import 'setuptools.build_meta'`** — a global
Python 3.15 (or any slim install) with no setuptools. `python -m venv .venv` and
install inside it; the SDK itself needs no build tooling at run time.

**`pip install -e` prints "Please be careful with folders in your working
directory with the same name as your package"** — setuptools noticing that the
source tree contains a directory called `omni` next to the `omni` package it is
building. Benign: the install succeeds, and the import resolves to the package.
Every editable install of a source-layout package with this layout warns.

**GitHub does not show "Omni"** — by design, and see section 5.3. Nothing in a
repository config can register a new language with Linguist.

**A Bengali or other non-ASCII comment makes a program fail** — it should not,
and there are tests in three languages for it (`"// নম্বর\n42"` evaluates to 42).
If it does, the source arrived with a BOM or invalid UTF-8: the ABI returns
`OMNI_ERR_ARG` for malformed UTF-8 rather than guessing, so check the encoding at
the call site.

---

## 10. What Phase 2 is

The memory model is built for things the syntax does not yet have. In rough order:

1. **Strings.** This is the change that turns `OmniValue` from a struct into a
   union with a pointer arm, and it forces the second soundness rule to be
   tested rather than asserted: the string must be **copied** into caller-owned
   storage, because the arena that holds it is freed on the next `exec`.
2. **Booleans, `nil`, `undefined` as literals.** The tags and singletons already
   exist in `Value`; only the parser is missing.
3. **An AST.** Extracting one from the current one-pass evaluator, so functions
   and control flow have something to be built from.
4. **Functions and control flow.** Which is where the arena's `reset`-per-exec
   model needs re-examining for call frames.
5. **wasm for real.** `wasm-bindgen`, the `wasm32-unknown-unknown` target, and a
   browser example; the shape is already scaffolded and asserted to agree with
   the C statuses.
6. **Linguist tier 2.** The upstream PR from section 5.3.

Deferred on purpose, not forgotten: no GC (there is nothing to collect), no
incremental compilation tricks beyond `incremental = true`, no dependency
graph to cache.
