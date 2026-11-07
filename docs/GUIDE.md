# OmniEngine — initialization and execution guide

Everything below was run on this machine, and every number is measured rather
than estimated. Where the design and the reality diverged, the reality is what
is written here, with the reason.

Original brief: a high-performance custom language and ecosystem in `D:\OmniEngine`,
lightweight and modular, with a Rust workspace, a C ABI that Python and
JavaScript can call, a custom file extension that GitHub recognises, and
SDKs that install through `pip` and `npm`.

Two numbering systems are in use and both are kept, because the brief's five
phases are how the work was asked for and how section 7 is scored. **"Phase 1–5"
means the brief's deliverables** (workspace, core, bridge, Linguist, SDKs).
**"The strings increment" or "ABI v2"** means the second round of work on the
language and the bridge — strings, the egress memory model, and the four new
exports. **"The callback increment" or "ABI v3"** means the third — host
functions, call syntax, and the re-entrancy guard. Neither is the brief's Phase 2
or Phase 3.

---

## 0. Status

| Deliverable | State | Evidence |
| --- | --- | --- |
| Phase 1 — workspace and build policy | done | `Cargo.toml`, `.cargo/config.toml`, `rust-toolchain.toml` |
| Phase 2 — core engine | done | 128 `omni-core` tests |
| Phase 3 — C ABI + header | done | 45 `omni-ffi` tests, PE export table, `examples/c/embed.c` |
| Phase 4 — extension and Linguist | done (tier 1) | `.gitattributes`, `examples/*.omni` |
| Phase 5 — SDKs and DX | done | 107 Python + 93 Node tests, CLI, wasm scaffold |
| Strings, egress memory, ABI v2 | done | 4 new exports, `omni_str_copy` + `omni_define_string` + `omni_debug_stats` + `omni_abi_version` tested from all four hosts |
| Host callbacks, re-entrancy, ABI v3 | done | `omni_register_host_fn` + `omni_unregister_host_fn` + `omni_host_fn_count`, a `RefCell` borrow guard on every entry point, and call syntax in the parser |
| Batch execution, benchmarks, ABI v4 | done | `omni_exec_many` + `omni_batch_error`, checkpointed arena reclamation, three zero-dependency benchmark harnesses and `scripts/bench.sh` (§4.8, §4.9) |
| Packaging, distribution, release | done | platform wheels with the engine inside, an npm tarball with five engines, `scripts/package.py`, `scripts/check-versions.py`, `.github/workflows/release.yml` (§6.6, `docs/RELEASING.md`) |
| Same answer from four hosts | done | `scripts/verify-all.sh` — 21 rows, all passing |

```
cargo test --workspace          # 225 tests
./scripts/verify-all.sh         # Rust, C, Python, Node: 42, "hello, world", ABI 4
./scripts/bench.sh              # three hosts, one crossing each, measured
python scripts/check-versions.py  # one version, one ABI across every surface
python scripts/package.py         # a clean native archive for this platform
```

---

## 1. Verified environment

Measured on 2026-11-07; toolchain versions are the ones 2026-11-03 recorded and
have not moved since.

| | |
| --- | --- |
| rustc | 1.99.1, host `x86_64-pc-windows-gnu` |
| cargo | 1.99.1 |
| gcc | 16.2.0 (MinGW-W64 x86_64-ucrt-posix-seh, WinLibs) |
| Python | 3.15.0 |
| Node / npm | 26.12.0 / 11.20.0 |
| **C:** | 100 GB total, **6.5 GB free (94 % used)** |
| **D:** | 138 GB total, **46 GB free (68 % used)** |
| `CARGO_HOME` | 982 MB on disk (`du`), 1,067 MB of file bytes — the registry is nearly all of it, and none of it is from this project |
| `D:\OmniEngine\target` | 351 MB (21 MB of it under `target/release`: the release library, the benchmark binary, and cargo's release deps) |
| `target/debug/omni.dll` | 5,630,962 bytes — 5.4 MB as `du -h` reports it |
| `target/release/omni.dll` | 1,289,387 bytes — the artifact a release package carries |
| Rust sources | 9,271 lines (`crates/**/*.rs`) |
| Python, JavaScript, and C sources | 6,792 lines — 2,996 Python, 3,312 JavaScript, 484 C (`sdks/` and `examples/c/`, `node_modules` excluded) |

**The storage premise needed correcting.** The brief said 20 GB allocated, and
the natural reading was "keep it small on D:". D: is not the problem. C: — where
`CARGO_HOME`, the toolchain, and every default build cache live — has 6.5 GB
free. That is the constraint that shapes the build policy, and it is why
section 2.4 exists.

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
│   ├── omni-core/             lib.rs value.rs arena.rs counted.rs object.rs lexer.rs engine.rs  (no deps)
│   ├── omni-ffi/              src/lib.rs, include/omni.h, cbindgen.toml,
│   │                          tests/{ffi_contract,host_callbacks}.rs
│   ├── omni-cli/              src/main.rs                            (no deps)
│   └── omni-wasm/             scaffold, excluded from default builds
├── sdks/
│   ├── python/                omni/{__init__.py,_ffi.py}, tests/, pyproject.toml
│   └── nodejs/                index.js, lib/{bindings,discover}.js, tests/
├── examples/                  hello.omni, arithmetic.omni, strings.omni, c/embed.c
├── scripts/verify-all.sh
└── docs/                      GUIDE.md, superpowers/specs/, superpowers/plans/
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
gigabytes for a workspace this size, and they land on the drive with 6.5 GB
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
`unsafe` sits in three files — `arena.rs`, `object.rs`, and `counted.rs` — none of
which is the lexer or the evaluator.

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
| `0x7FF7` | **pointer** | 48-bit address of a value box, in the arena or the egress block |
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

- Every chunk is exactly `DEFAULT_CHUNK` (64 KiB). There is no growth rule: a
  request larger than a chunk gets its own exact-size block instead, and
  `MIN_CHUNK` (4 KiB) is only the floor the constructor insists on. A 16 MiB
  string therefore costs 16 MiB in one oversize block, not a silently resized
  arena — pointer stability stays a property of the layout rather than of luck.
- Allocation is a pointer bump. There is no per-object bookkeeping, so
  allocation costs are a compare and an add.
- Each chunk stores the `Layout` it was allocated with, and frees itself with
  that same layout. Reconstructing a `Box<[u8]>` from a 16-byte-aligned
  allocation would deallocate with alignment 1, which is undefined behaviour —
  this is the classic bump-arena mistake and it is why `Chunk` holds its layout.
- **`reset()` reclaims every chunk but one**, in O(chunks), and keeps that first
  chunk warm so the next execution's first allocation lands at the same address
  the very first one did. This is the "no GC" story: nothing is traced, so there
  is no pause and no collector to tune.

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
"unsafe is confined" claim checkable: raw pointers are dereferenced in three
files — `arena.rs` for the bump, `object.rs` for the one `unsafe fn write_at` that
lays out a string block, and `counted.rs` because `GlobalAlloc` is an `unsafe`
trait. The evaluator has none at all (`grep -c unsafe crates/omni-core/src/engine.rs`
prints `0`), and the lexer's two occurrences are both inside its own test module.

The invariant the facade depends on, and that the engine upholds: **every
collection of `Slot`s must be dropped before the arena is reset.**
`reclaim()` resets the arena and clears the bindings vector in the same call, so
the invariant cannot be broken by a caller — and `exec_many` does the same
pairing per item, with `reset_to(mark)` in place of a full reset (4.8).

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

The core deliberately has **no AST type**. `Engine::exec` lexes once and then
walks the token stream while evaluating, because an expression language with no
user-defined functions has nothing to reuse a tree for. Adding an AST later is a
refactor-with-a-home (section 10), not a rewrite.

```
program  := ( stmt )*                      // ';' is an optional separator
stmt     := 'let' IDENT '=' expr | expr
expr     := additive ( ( '==' | '!=' ) additive )*
additive := term ( ( '+' | '-' ) term )*
term     := factor ( ( '*' | '/' | '%' ) factor )*
factor   := ( '-' | '+' )? primary
primary  := NUMBER | STRING | call | IDENT | '(' expr ')'
call     := IDENT '(' ( expr ( ',' expr )* )? ')'      // <= MAX_ARGS arguments
```

The `IDENT '('` lookahead is what separates a call from a name: `pair` reads a
binding and `pair()` reads the host function registry, so a host may install both
under one name and the grammar tells them apart without a second namespace. The
rule sits at the *primary* level, which is why a call binds tighter than every
operator — `square(6) + 1` is 37, not a parse error.
Precedence comes from the recursive descent, left-to-right, so `2 + 3 * 4` is
14 and `(2 + 3) * 4` is 20 — both tested. Comparison is one level looser than
`+`, which is what makes `"a" + "b" == "ab"` parse as `("a" + "b") == "ab"`; the
two-level grammar it replaced could not express that at all.

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

### 3.5 Strings: three blocks, three lifetimes

A string value is still eight bytes. The `0x7FF7` pointer row addresses a box
that holds `{ len, bytes }`, and the bytes themselves live in one of three
places, each with its own lifetime. That split is the whole design:

| Block | What lives there | Released | Visible to a host |
| --- | --- | --- | --- |
| exec arena | intermediates: a literal as the lexer decoded it, a temporary operand | `reset()` at the start of every `exec` | never |
| egress | anything a result may point at, copied out of the arena before `exec` returns | `rewind()` — the cursor goes to zero, nothing is unmapped | yes, until the next `exec` |
| host store | bytes installed by `omni_define_string`, one `Box<[u8]>` per global | when the binding is replaced or the engine is freed | yes, for the engine's whole life |

Because egress rewinds instead of freeing, **a stale string address is a wrong
read, not a crash**. `embed.c` asserts it from the C side: a 12-byte result
copied with a 32-byte buffer after a 27-byte execution comes back as
`"a much longer second result"` — the *newer* bytes at the *newer* length,
because `omni_str_copy` sizes the copy from the string that is actually sitting at
that address and never trusts the stale struct's `len`. Both SDK suites assert the
same fact from the other side: with a stale `len` used as the caller's cap,
`"first"` reads back as `a muc`, a prefix of whatever now lives there. That is the
documented trade, and it is why every host copies before it caches.

The lexer decodes `"\n"`, `"\r"`, `"\t"`, `"\""`, `"\\"` and `"\u{…}"`. A `\u{…}`
must be one to six hex digits and a legal scalar: `D800..DFFF` and anything above
`10FFFF` is a compile error naming the code point, so no engine string is ever
invalid UTF-8 (a Rust `String` holds it, which is the guarantee that makes that
true rather than a hope). A literal may not contain a raw newline. Strings are
length-delimited, so an interior NUL is data — `Result.text` in Python and
`Result.text` in Node are byte buffers, not C strings. `MAX_STRING_LEN` is 16 MiB.

`+` concatenates and refuses a mixed-type program by naming both operands:

```
line 1: cannot add string to number: "hi" + 42
```

`==` and `!=` compare content, not addresses. There is no implicit coercion in
either direction: `42 + "hi"` is `cannot add number to string: 42 + "hi"`, and a
boolean reaches the same arm as `cannot add boolean to string: false + "hi"` — all
three messages asserted verbatim in `engine.rs`. `verify-all.sh` then checks that
the `"hi" + 42` one arrives in Rust, C, Python, and Node with identical bytes.

The counting allocator turns "no GC" into something a host can measure rather
than a claim. `omni_debug_stats` fills a `#[repr(C)]` struct with `execs`,
`strings`, arena in-use/reserved/chunks, and egress in-use/reserved/high-water.
Measured here, 5,000 consecutive string executions of `"hello, " + "world" +
"\u{1F600}"` (30,000 strings built):

| Counter | Warm (200 execs) | After 5,200 execs |
| --- | --- | --- |
| `execs` | 200 | 5,200 |
| `strings` | 1,200 | 31,200 |
| `arena_bytes_in_use` / `reserved` | 97 / 65,536 | 97 / 65,536 |
| `arena_chunks` | 1 | 1 |
| `egress_bytes_in_use` / `reserved` | 25 / 65,536 | 25 / 65,536 |
| `egress_high_water` | 25 | 25 |

Two numbers do not move, which is the point: memory is bounded by the largest
single execution, not by how many you ran. `strings` is the counter that is
supposed to grow.

### 3.6 What the core still does not have

No string length, indexing, slicing, methods, or interpolation; no ordering
comparisons on strings; no boolean, `nil`, or `undefined *literals*` (booleans
exist as values, and only `==` / `!=` produce them); no control flow or
collections. There are no *user-defined* functions either — a call site can only
name a function the host registered (4.7), which is what makes the grammar's new
`IDENT '(' args? ')'` rule complete rather than half-built. `Engine::exec` is
still the only entry point, and it still takes `&str`.

---

## 4. Phase 3 — Universal bridge and FFI layer

### 4.1 What a build produces

```bash
cargo build -p omni-ffi
```

Verified output in `target/debug/` on this machine:

| File | Purpose |
| --- | --- |
| `omni.dll` (5.3 MB) | what Python, Node, and `LoadLibrary` load |
| `libomni.dll.a` | MinGW import library, for C link time |
| `libomni.a` | static archive |
| `libomni.rlib` | lets the Rust contract tests call the same surface |

The library is named `omni`, not `omni_ffi`, via `[lib] name = "omni"` — so the
artifact a host loads matches `omni.h` and what the SDKs look for. That was a
correction: the manifest originally promised `omni.dll` and produced
`omni_ffi.dll`.

The exports are not assumed. `objdump -p target/debug/omni.dll` shows exactly
fourteen, and that is the check:

```
omni_abi_version       omni_debug_panic       omni_debug_stats
omni_define_string     omni_engine_free       omni_engine_new
omni_exec              omni_exec_number       omni_host_fn_count
omni_last_error        omni_register_host_fn  omni_str_copy
omni_unregister_host_fn                        omni_version
```

The first four additions are the strings increment: the ABI gate, the copy-out,
the host-defined string, and the stats read. The last three are the callback
increment: bind a host function, drop it, and count what is bound.

### 4.2 The four rules that make this sound

**A pointer leaves the engine only as an integer nobody may dereference.** The
arena is bulk-freed at the start of every `exec`, so a `Value` pointing into it
would dangle on the next call. Results therefore leave as a 16-byte
`{ tag, len, payload }` struct **written through a pointer the caller owns** —
`omni_exec(e, src, len, &out)` — never as something a host has to free, and
`omni_str_copy` takes that same struct by `const OmniValue *` so the 16 bytes
never cross a calling convention in three toolkits either. The `ptr` arm is
opaque to every host: nothing in `omni.h`, `ctypes`, or `koffi` dereferences it.
The one way to get at the bytes is `omni_str_copy`, which re-derives the address
against the engine's own blocks and copies into storage the caller owns. What a
host may point at is promoted into the egress block first (section 3.5), which is
what makes the copy legal to perform at all. The first bridge design stated this as
"no arena pointer crosses the boundary"; the strings increment retired that wording in
favour of this one, because an address does now cross — it simply cannot be used.

**A Rust panic may never unwind into a host.** Unwinding past `extern "C"` is
undefined behaviour, and it is the most common defect in hand-rolled Rust FFI.
Every body runs inside `catch_unwind(AssertUnwindSafe(..))` and a caught panic
becomes `OMNI_ERR_PANIC`. There is a test for it, driven from both sides:
`omni_debug_panic` returns 3, and the *same* engine then evaluates `1 + 1` to 2.

**The wire format is checked before it is used.** `omni_abi_version()` returns 3,
and every host compares it with its own constant *first*: `embed.c` exits before
creating an engine, and both SDKs refuse to finish loading. A stale artifact read
with the new layout does not error — `len` and the payload swap meanings and the
library returns plausible wrong numbers. Version 1 was `{ tag, _pad, number }`;
the two are the same size, so nothing but this check distinguishes them.

**The engine is borrowed, not assumed free.** A host callback runs *inside* an
evaluation, so anything it calls back into the same handle would take a second
mutable borrow of live state. `OmniEngine` holds its `Engine` in a `RefCell`, and
every entry point opens it with `try_borrow_mut()`: a re-entrant call answers
`OMNI_ERR_REENTRANT` (6) and touches nothing, where the unchecked version of this
design would have been two `&mut` aliases and a segfault that looks like a bug in
the host's own code. It is also what makes section 4.7's borrowed arguments safe
to read: while a callback holds them, nothing else can move the engine.

### 4.3 The surface

| Function | Returns |
| --- | --- |
| `omni_abi_version()` | `4` — read this before anything else |
| `omni_version()` | static NUL-terminated string, never freed |
| `omni_engine_new()` | opaque `OmniEngine*`, null on failure |
| `omni_engine_free(e)` | nothing; null is a no-op |
| `omni_exec(e, src, len, out)` | status; writes `OmniValue` |
| `omni_exec_many(e, srcs, lens, n, out, statuses)` | status of the first failing item; writes `n` values and `n` per-item statuses (4.8) |
| `omni_batch_error(e, index, buf, cap)` | bytes copied for item `index` of the current batch; `0` once the next execution has expired it |
| `omni_exec_number(e, src, len, out)` | status; writes a `double`; refuses a non-number tag |
| `omni_str_copy(e, value, buf, cap)` | bytes copied; `0` for a non-string tag or an address this engine does not own |
| `omni_define_string(e, name, name_len, bytes, bytes_len)` | status; installs a global the language can read |
| `omni_register_host_fn(e, name, name_len, arity, fn, user_data)` | status; binds a function a script can call (4.7) |
| `omni_unregister_host_fn(e, name, name_len)` | entries removed, `0` or `1` |
| `omni_host_fn_count(e)` | registrations the engine currently holds |
| `omni_debug_stats(e, out)` | status; fills `OmniStats`, whose `size` field says which layout was written |
| `omni_last_error(e, buf, cap)` | bytes written, excluding the NUL |
| `omni_debug_panic(e)` | status — a test hook, not an API |

Statuses: `0 OK`, `1 ERR_COMPILE`, `2 ERR_RUNTIME`, `3 ERR_PANIC`, `4 ERR_ARG`,
`5 ERR_HOST`, `6 ERR_REENTRANT`. Tags: `0 NUMBER`, `1 NIL`, `2 BOOL`, `3 UNDEF`,
`4 STRING`. `ERR_HOST` is what a callback's own failure becomes when the script
that called it fails; `ERR_REENTRANT` is the guard in 4.2. Since ABI v4,
`OmniStats.execs` counts batch **items** — executions — not crossings, because a
counter that counted crossings could not verify the linearity claim in 4.9.

The layout is pinned on both sides so a drifting SDK fails at compile time
rather than returning wrong numbers at run time:

```rust
const _: () = assert!(std::mem::size_of::<OmniValue>() == 16);
const _: () = assert!(std::mem::offset_of!(OmniValue, tag) == 0);
const _: () = assert!(std::mem::offset_of!(OmniValue, len) == 4);
const _: () = assert!(std::mem::offset_of!(OmniValue, payload) == 8);
const _: () = assert!(std::mem::size_of::<OmniStats>() == 72);
```

with `_Static_assert` in `omni.h`, nine `ctypes` offsets and sizes checked at
Python import, and nine `koffi` offsets and sizes checked at Node load. Four
independent assertions of the same layout is the point: the Rust one catches the
crate drifting, the C one catches the header, and the two SDK ones catch a host
that was installed from an older tag.

`OmniValue` is `{ u32 tag, u32 len, union { double number; const char *ptr } }`.
The union was the spec's shape and it is now the real one: `ptr` for `STRING`, with
`len` as its byte count, and `number` for everything else — the double itself for
`NUMBER`, `1.0` or `0.0` for `BOOL`, `0.0` for `NIL` and `UNDEF`. A non-string
always has `len` 0, and no arm is ever left holding raw engine bits that could be
mistaken for a value: the tag alone says which of the two to read.

### 4.4 Four ABI details that only showed up under testing

**`omni_last_error` copies the rendered error, not just the message.** The first
version copied `error.message`, so a host received `expected a name` with no line
and no way to point at the offending token. It now copies the `Display` form
whole — `line 1: expected a name, found ''=''` — and the contract test asserts the
line number survives the boundary. Note the doubled quotes around `=`: that is the
engine, not the SDK. `display_kind` renders a symbol as `'='` and the message
template wraps it in quotes a second time. The hosts pass the text through
untouched rather than prettifying it, and section 10 records the one-character
fix.

**A zero-length program is legal even from a null pointer.** Node hands an empty
`Buffer` to C as NULL, and `slice::from_raw_parts(null, 0)` is undefined
behaviour even though it reads nothing. So `omni_exec(e, NULL, 0, &out)` now
short-circuits to the empty program and returns `UNDEF`, while
`omni_exec(e, NULL, 5, &out)` is still `ERR_ARG`. The same rule covers
`omni_define_string`'s two buffers.

**`omni_str_copy` validates the address instead of trusting it.** The `ptr` arm
of an `OmniValue` is 48 bits the caller can write, so "is this one of mine" is
answered by looking the address up in the three places the engine actually owns:
the egress block, the current execution arena, and a live host binding. A
fabricated pointer, a superseded host string, or another engine's value copies
zero bytes rather than reading somebody else's heap — and a pointer with bits
above 48 is rejected before anything masks them off. `len` in the struct is only
a sizing hint: `omni_str_copy` never reads it, because the byte count comes from
the engine's own box and is then bounded by the caller's `cap`. A host that hands
back a doctored `len` still gets real bytes.

**Every buffer length is an explicit argument.** The spec had
`omni_define_string(engine, name, bytes)`; the shipped form is
`omni_define_string(engine, name, name_len, bytes, bytes_len)`. A
NUL-terminated name cannot express a zero-length one, and a length-delimited
payload is the thing the engine already guarantees, so the API matches the model
instead of forcing hosts to invent a terminator. The C header and both SDKs
follow it.

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
    examples/c/embed.c target/debug/libomni.dll.a -o target/debug/embed
cd target/debug && ./embed
# 39 checks: 42, strings, escapes, host strings, staleness, stats, callbacks, panic
```

The output goes into `target/debug` because that is where `omni.dll` is: Windows
resolves an import table against the executable's own directory first, so the same
binary built into the repository root stops at
`error while loading shared libraries: omni.dll` and never reaches `main`.
`verify-all.sh` copies the library next to the executable for the same reason. The
`thread '<unnamed>' panicked at ... omni_debug_panic` line on stderr is the panic
test doing its job; the last line on stdout is the pass marker.

The memory section is the one that matters for the bridge: it reads stats before
and after 5,000 string executions and asserts that neither reservation moved,
then asserts `omni_debug_stats(engine, NULL)` is `OMNI_ERR_ARG` rather than a
write through a null pointer.

### 4.6 Concurrency

One handle, one thread. `&mut Engine` is what the ABI assumes internally, and
the SDKs document the rule instead of pretending to enforce it. `Send` is
implemented for the arena types because the handle moves cleanly between
threads — it is just not shared.

### 4.7 Host callbacks: the bridge running backwards

Everything above hands a value to a host. A host callback hands control *back* —
the engine reads a call in a script, enters foreign code, and then trusts whatever
that code writes. Three exports and four rules make that trustworthy.

```c
int32_t omni_register_host_fn(OmniEngine *engine, const char *name, size_t name_len,
                              uint32_t arity, omni_host_fn callback, void *user_data);
size_t  omni_unregister_host_fn(OmniEngine *engine, const char *name, size_t name_len);
size_t  omni_host_fn_count(const OmniEngine *engine);
```

The grammar needed one addition: `IDENT '(' args? ')'`, distinguished from a plain
name binding by the parenthesis, which is why a host string and a host function may
share a name (`pair` can be both a variable and a callable). Arity is fixed at
registration and checked by the engine *before* the host is entered, so a callback
never sees a call it was not written for, and a script that gets the count wrong
reads `host function 'square' takes 1 argument, got 2`. `MAX_ARGS` is 16 and
`MAX_CALL_DEPTH` is 64, both enforced in the evaluator rather than documented and
hoped for.

**Rule 8 — arguments are borrowed, and a string argument's pointer is the text.**
A *result* struct's `payload.ptr` addresses the engine's own object header, which is
why `omni_str_copy` takes that struct. An *argument* struct's `payload.ptr` addresses
the first byte of the payload itself, and both are readable only until the callback
returns. So a callback reads its arguments with a plain copy — `string_at` in
`ctypes`, `koffi.decode` in Node, a loop in C — and never with `omni_str_copy`, which
would re-enter the engine that is already inside the call. Returning a string is the
mirror of the same rule: the callback puts the address of *its own* bytes in `out`,
and the engine copies them into egress before it uses them, so the buffer may be
reused the moment the callback returns.

**Rule 9 — re-entrancy is refused, not supported.** That is the `RefCell` guard in
4.2, and it is the reason rule 8's borrowed view is sound: the arena is reset at the
entry of `omni_exec()`, and a nested call can never reach that entry. Where a status
cannot be returned — `omni_str_copy`, `omni_unregister_host_fn`, `omni_last_error` —
the answer is `0`, because a `size_t` and a `bool` carry no error code.

**Rule 10 — nothing may unwind; a callback fails by returning 1.** A Python
exception or a JavaScript `throw` that crossed into Rust would unwind through foreign
frames, undefined behaviour in both directions. Both SDK trampolines catch
`BaseException` / any throwable, put the message in `out` as an `OMNI_STRING`, and
answer `OMNI_HOST_ERR` — `line 1: host function 'boom' failed: ValueError: host said
no`. The host's own stack is gone by then, so the text is all either side keeps,
which is why the SDKs prefix the exception class.

**And one rule the header cannot write: the host owns the trampoline's life.** A
registration outlives the call, so `ctypes` and `koffi` each free their closure the
moment nothing references it — which would be the line after registration. Each SDK
therefore keeps the closure alive for exactly as long as the engine holds its
address: a map on the `Engine`, released by `unregister_function` and then by
`close()`, in that order. Redefining a name replaces the callback without freeing
anything, which is safe because the engine never owned it in the first place.

From Rust the whole surface is contract tests
(`crates/omni-ffi/tests/host_callbacks.rs`); from C it is `embed.c`; from a host it
is the four `verify-all.sh` rows that only make sense across two languages at once —
identical bytes from a callback, identical failure text, a re-entrant call refused
with the engine still usable afterwards, and the registry count agreeing.

### 4.8 Batch execution: one crossing, many programs

`omni_exec_many` carries N programs in one crossing, amortising the crossing and
the arena reclamation over the whole list. The header's rule 11 is the contract
in one sentence: **a batch is N independent executions sharing one egress
rewind**, not one long program — item *N* cannot read item *N−1*'s `let`.

The mechanics behind that sentence, each checkable in `omni-ffi/tests/batch.rs`
and `embed.c`:

- **One reclamation, then N isolated runs.** `reclaim()` runs once, before the
  first item; between items the arena resets back to a `Mark` taken at batch
  start and the bindings vector clears. Each item is exactly one `exec`'s worth
  of state.
- **All N results are live at once.** The egress block rewinds once, before the
  first item, so every string result stays readable until the next `omni_exec`
  or `omni_exec_many` — a host can copy all of them out after the call. The
  trade, stated not hidden: reserved egress bytes are bounded by one batch's
  total demand rather than one execution's. 256 string items measured 6,142
  bytes of high-water against a reservation that never moved.
- **Two error channels.** A malformed *call* — null arrays, `count > 4096`, a
  source over 256 MiB, bad UTF-8 — is refused whole: `OMNI_ERR_ARG` and nothing
  written. A failing *item* is data: every element of `statuses` is written,
  the return status is the first item's failure, and `omni_batch_error(index)`
  copies that item's own message — until the next execution, at which point it
  answers zero bytes rather than a stale one. `OMNI_MAX_BATCH` is 4096.
- **The guards hold.** A batch attempted from inside a host callback is
  `OMNI_ERR_REENTRANT` exactly like a single exec, and a host failure inside an
  item is that item's status, never an unwind.

The SDK surfaces: `engine.run_many([...]) -> list[Result]` (raises naming the
first failing item) and `engine.try_run_many([...]) -> list[(status, result,
message)]` in Python; `engine.runMany([...])` / `engine.tryRunMany([...])` in
Node. Both SDKs refuse an over-long list locally, before the crossing. Usage
advice that the measurements below earn: a batch of one is strictly a loss —
one extra list is still extra work — and the win saturates fast; there is no
reason to build huge lists for the crossing's sake.

### 4.9 Measured performance: the three harnesses

Three zero-dependency harnesses measure what each host can see with its own
instruments — the counting `GlobalAlloc` in the Rust binary, the `gc` callbacks
and `tracemalloc` in Python, the `PerformanceObserver` in Node — and
`scripts/bench.sh` runs whichever are present and prints the comparison. The
method is shared so the tables mean the same thing: five rounds, fastest
reported (a scheduler preemption only ever adds time), separate passes for
timing and counting, and the engine's own `execs` counter checked per timed
pass, so a round that skipped executions fails loudly.

Canonical run, this machine, 20,000 iterations, `./scripts/bench.sh`
(2026-11-07; full tables land in `target/bench/`):

```
host           string ns   empty crossing   best batch/item
rust              269.5              --              0.99x
python          10062.0           239.1              1.24x
node             8489.9            35.8              1.38x
```

The three tables agree on the headline: **the crossing costs 36–240 ns and the
wrapper costs microseconds, so a batch's time win at the SDK level is bounded
(1.2–1.4× per item at best), while its allocation win is unconditional — in
Rust, 0.00 allocations per execution in every scenario, falling to 0.004 per
*item* inside a 256-batch.** The empty-crossing row is why: ctypes spends
239 ns on a call that does nothing, koffi 36 ns, and that is the entire thing a
batch can amortise. The rest of each call is the host building arguments and
reading results, which a batch cannot remove — it can only share the per-item
part of the wrapper, which is where the SDK speed-ups come from.

The linearity claim, checked in all three harnesses at 1,000 / 10,000 /
100,000 executions of the `string` scenario: per-execution time flat (Rust
225.9 → 229.8 → 240.3 ns; Node 7.3 → 8.2 → 8.4 µs; Python flat within its
noise), egress reservation 65,536 bytes throughout, `execs` counted exactly,
retained bytes zero. Every number in this section is a quoted run, not a
promise — the harnesses print the library paths and tool versions they ran
against, and a laptop's noise floor is part of the record (the Python batch
table's N=256 cell measured 0.96× on this run and 1.34× on another, and says
so).

Two core changes the baseline earned, and one thing deliberately not done:
the token vector is now reused across executions (a warm engine lexes without
asking the allocator for anything), and the arena's cursor walks already-mapped
chunks instead of mapping fresh ones per cycle — the change without which
"reservation flat over 100,000 executions" could not be written. Deliberately
not done: parallelism inside a batch (a sequential batch has no
synchronisation to get wrong), and bytecode or caching (an expression
language with no user-defined functions has nothing to reuse a compiled form
for).

---

## 5. Phase 4 — GitHub Linguist and file extensions

### 5.1 Extensions

`.omni` is the primary extension; `.ox` is the short alias. Both are declared in
`.gitattributes` with `eol=lf`. `examples/` holds three `.omni` programs —
`hello.omni`, `arithmetic.omni`, and `strings.omni` — and no `.ox` file yet, so
the alias is a rule that is currently untested by anything in this repository.

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
"without altering native project statistics" true *in this repository*: the 5,858
lines of Python and JavaScript under `sdks/` — 2,799 and 3,059 — do not dilute the
9,271 lines of Rust. The numbers are raw `wc -l`; Linguist reports fewer for both,
because it drops blanks and comments, and the ratio is not the claim — the
*exclusion* is, so no SDK line reaches the bar at all. `examples/c/embed.c` is
deliberately *not* vendored: it is part of the engine's own proof, and its 484
lines against 9,271 would not move the bar anyway.

### 5.3 The honest limit, and the two-tier plan

The brief assumed `.gitattributes` could make GitHub display a chosen language
name. It cannot. Linguist only maps extensions onto languages it already knows;
`linguist-language=Anything` is validated against its own grammar list, and an
unknown value is ignored. There is no supported per-repository custom language.

So the plan has two tiers, and only the first one is a configuration file:

**Tier 1 — works today, committed.** `.omni` and `.ox` files are attributed to
Rust, which is true at this stage: an `.omni` file contains an expression language
whose entire implementation is the Rust in this repository. The
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

Both SDKs are thin, both are stdlib-only or nearly so. A **release package
carries the engine**, built from the same tag in the same CI run; a **checkout
install** uses the library you built locally. Section 6.3 lists where each one
looks, and section 6.6 describes the packaging itself.

### 6.1 Python

```bash
# From PyPI, once 0.1.0 is published: a wheel with the engine inside.
pip install omniengine

# From a checkout, editable, using the local build:
cargo build -p omni-ffi
python -m venv .venv && source .venv/Scripts/activate   # bash on Windows
pip install -e sdks/python

# Build the release wheel yourself; setup.py stages the artifact:
OMNI_NATIVE=build python -m build --wheel sdks/python
```

Neither package is on PyPI or npm yet. What was verified locally: a wheel built
with `OMNI_NATIVE=<built lib>` and installed into a clean venv **outside the
repository**, with no `OMNI_*` variables, evaluates `40 + 2` to `42.0` and runs
a batch — it finds `omni/_native/omni.dll` inside its own package. The registry
form is now a build-and-upload step (`docs/RELEASING.md`), not an open question.

```python
from omni import Engine

with Engine() as engine:
    print(engine.evaluate("let answer = 40 + 2"))   # 42.0
    result = engine.run("2 + 3 * 4")
    print(result.tag, result.value)                  # number 14.0
    print(engine.string(r'"omni" + " " + "\u{1F600}"'))   # omni 😀
    engine.define_string("greeting", "hello wörld")       # lives until redefined
    print(engine.run('greeting + "!"').value)             # hello wörld!
    print(engine.stats().egress_bytes_reserved)           # 65536
```

A script can also call back into Python:

```python
with Engine() as engine:
    engine.register_function("shout", lambda s: s.upper() + "!", arity=1)
    engine.register_function("now", lambda: 1735689600.0, arity=0)
    print(engine.string('"say: " + shout("hey")'))   # say: HEY!
    print(engine.host_function_count)                 # 2
    engine.unregister_function("now")                 # the script stops seeing it
```

Arguments arrive as `float` / `bool` / `str` / `None` and the return value goes
back the same way; anything else, and any exception, becomes a readable host error
rather than a crash (section 4.7). `arity` is required because a Python signature is
not reliably knowable, and it is checked by the engine before your callable is
entered.

`run` returns a frozen `Result` (`tag`, `number`, `text`, `value`); `evaluate`
returns a `float` and raises `OmniError` on any non-number or failure; `string`
returns a `str` and raises on any non-string; `try_run` returns
`(status, result | None, message)` and never raises. `close()` is idempotent and
`__del__` is guarded by an `atexit` hook, so an engine that outlives the script
is not a use-after-free.

`Result.text` is the exact `bytes` of a string result — a NUL inside a string is
data, not a terminator — and `Result.value` is those bytes decoded as UTF-8.
`Result.number` is `0.0` for a string on purpose: those eight payload bytes are
an engine address, and `float`-printing one would be noise in a log. The copy
happens inside `run`, before anything can rewind the block it points into; the
SDK never hands out an address, so there is nothing for a caller to hold past its
lifetime.

`_ffi.py` checks nine `ctypes` sizes and offsets at import and compares
`omni_abi_version()` against its own constant, so an engine built from an older
commit raises at import with `cargo build -p omni-ffi` in the message instead of
returning numbers read from the wrong offsets.

The dependency list in `pyproject.toml` is empty and that is load-bearing: the
binding is `ctypes`. `cffi`, `pybind11`, and a build-time compiler would each add
an install failure mode to a package whose entire job is to find one DLL.

License metadata uses PEP 639 (`license = "MIT"` plus `license-files`), which
needs `setuptools>=77`; the table-form `license = {text = "MIT"}` emits a
deprecation warning on current setuptools.

### 6.2 Node.js

```bash
# From npm, once 0.1.0 is published: one tarball, five platform engines.
npm install omniengine koffi

# From a checkout, where the package uses the local target/ build:
npm install ../sdks/nodejs        # a consumer project installing from this repo
npm install                       # inside sdks/nodejs, for the SDK's own tests
```

```js
const { Engine } = require('omniengine');

const engine = new Engine();
console.log(engine.evaluate('let answer = 40 + 2'));   // 42
console.log(engine.run('2 + 3 * 4').value);            // 14
console.log(engine.string('"omni" + " " + "\\u{1F600}"'));  // omni 😀
engine.defineString('greeting', 'hello wörld');
console.log(engine.run('greeting + "!"').value);       // hello wörld!
console.log(engine.stats().egressBytesReserved);       // 65536
engine.close();
```

A script can also call back into JavaScript:

```js
const engine = new Engine();
engine.registerFunction('shout', (s) => s.toUpperCase() + '!', { arity: 1 });
engine.registerFunction('config', () => ({ retries: 3 }), { arity: 0 });
console.log(engine.string('"say: " + shout("hey")'));  // say: HEY!
console.log(engine.hostFunctionCount);                  // 2
console.log(engine.tryRun('config()').message);
// line 1: host function 'config' failed: a host function must return a number,
//         bool, string, Buffer or null, not object
engine.close();                                        // releases both trampolines
```

The last call is the point of the example: an object is not a value the ABI can
carry, so the SDK reports the shape rule as a host error the script reads with
status `5` (`host`), instead of coercing `{}` to a number and letting the program
continue on a lie.

`run` → `Result` (`tag`, `number`, `text`, `value`, where `text` is a `Buffer`);
`evaluate` → number; `string` → string; `tryRun` → `{ status, statusName, result,
message }` with no throw; `stats` → plain JS numbers, never BigInt. `Symbol.dispose`
is wired onto the prototype, so `using engine = new Engine()` frees the handle at
the closing brace — run on Node 26.12, in CommonJS, no flag. It is defined
conditionally because the symbol does not exist on older runtimes.

`koffi` is an **optionalDependency** and `dependencies` is empty. If koffi has no
prebuild for a platform, `npm install` still succeeds and the failure surfaces as
an `OmniLibraryError` with an actionable message when you construct an `Engine`,
rather than as a broken install. koffi 3.3.6 was verified working on Node 26 with
npm's install script blocked, which is the common hardened-CI configuration.

The published tarball carries one directory per platform:
`native/win32-x64/omni.dll`, `native/linux-{x64,arm64}/libomni.so`, and
`native/darwin-{x64,arm64}/libomni.dylib`. `postinstall` loads the one for the
current machine through the same `loadBindings` the API uses, so the ABI and the
struct layout are checked before the first script runs, and it never downloads
anything. A missing artifact is a warning unless `OMNI_STRICT_POSTINSTALL=1`,
because that is the normal state of a checkout install; `OMNI_SKIP_POSTINSTALL=1`
skips the check entirely.

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
| 3 | `<package>/_native/<lib>` (a release wheel) | `<package>/native/<platform>-<arch>/<lib>` (a release tarball) |
| 4 | next to the module (a wheel that puts it there) | next to `index.js` |
| 5 | upward from the package, `target/{debug,release}` at 6 levels | same |
| 6 | upward from the current working directory | same |

The upward walks exist because an installed checkout sits in `site-packages` or
`node_modules`, nowhere near `D:\OmniEngine\target\debug`. The first version of
the discovery code resolved the package's own location and found the temporary
directory the venv happened to live in — `…\Temp\omnivenv\target\debug` — which
was correct code answering the wrong question. Walking from the package *and*
from the working directory is what makes an editable install, a linked install,
and a run from inside the repo all work with no configuration.

One subtlety worth keeping: the walk checks the starting directory *before*
ascending. A version that ascended first could not find `<repo>/target/debug`
when run from the repository root, which is the most common case of all. There
are tests for both of these, in Python and in Node.

Measured, with no environment variables set:

| How it was installed | Where the script runs | Result |
| --- | --- | --- |
| `pip install -e sdks/python` | anywhere | found — the package lives in the repo, so the walk reaches `target/` |
| `pip install -e sdks/python` | outside the repo | found — editable, so the package still lives in the repo |
| `pip install dist/omniengine-0.1.0-py3-none-win_amd64.whl` (release wheel) | outside the repo | found — `omni/_native/omni.dll`, verified 2026-11-07 |
| `npm install ../sdks/nodejs` | anywhere | found — npm symlinks `file:` installs, Node resolves the real path |
| extracted `omniengine-0.1.0.tgz`, koffi on `NODE_PATH` | anywhere | found — `native/win32-x64/omni.dll`; `postinstall` verified ABI 4 |
| both | anywhere, `OMNI_ENGINE_HOME=D:\OmniEngine` | found |

If nothing is found, the error prints the build command, the list of paths it
tried, and the loader's own message. The stale-binary concern that shaped the
first four phases still decides *what* gets bundled: a checkout never stages
`target/debug`, and a release builds the artifact from the tag being released in
the same workflow run. The failure mode that argument protected against — a
wrapper carrying an engine from some other commit — is prevented by the build,
not by refusing to ship one.

### 6.4 The CLI

```bash
cargo run -p omni-cli -- run examples/hello.omni      # 42
cargo run -p omni-cli -- run examples/strings.omni    # hello, world 😀
cargo run -p omni-cli -- eval "2 + 3 * 4"             # 14
```

Exit codes are 0 success, 1 program error, 2 usage or I/O error, so a shell
script can tell "your expression is wrong" from "you invoked me wrong". Argument
parsing is hand-rolled; `clap` would add a dozen crates and a few hundred MB of
build cache for three subcommands.

A string result prints as its text, through `Engine::render` rather than
`Value`'s own `Display` — which deliberately prints `<string>` for a box it
cannot prove is live, because following an address is something only the engine
that owns the bytes may do. The CLI is exactly where a program's string result
should be readable, so it asks the owner. That distinction is a test
(`a_string_result_prints_its_content`), not a comment.

### 6.5 The wasm scaffold

`crates/omni-wasm` is a workspace member but not a default member, so
`cargo build` never needs `wasm32-unknown-unknown`, which is not installed here.
It mirrors the C ABI shape — number in, tagged result out, errors as strings,
and a `status_code` map asserted to agree with the C integers — so that adding
`#[wasm_bindgen]` later is a decorator pass over a surface that is already
tested, not a redesign.

```bash
cargo check -p omni-wasm                                        # works today
cargo build --target wasm32-unknown-unknown -p omni-wasm        # needs the target
```

`wasm-bindgen` is deliberately absent from `Cargo.toml`. Declaring a dependency
that cannot compile on this machine would put an unusable entry in `Cargo.lock`.

### 6.6 Packaging: what a release contains

Three artifacts, and each one carries the engine it needs:

| Artifact | Shape | Built by |
| --- | --- | --- |
| Python wheel | one per platform, `omni/_native/<lib>` inside | `setup.py` + `build`, retagged |
| npm tarball | one for all platforms, `native/<platform>-<arch>/<lib>` ×5 | `npm pack`, `prepack` guard |
| C archive | one per target, `lib/`, `include/`, `examples/c/`, README, LICENSE | `scripts/package.py` |

**The wheel.** `pyproject.toml` is metadata and nothing else; `setup.py` exists
to put a Rust artifact inside a Python package, which setuptools cannot do by
itself. A `build_py` subclass stages the library after the `.py` files land, and
`OMNI_NATIVE` chooses where it comes from:

| `OMNI_NATIVE` | Behaviour |
| --- | --- |
| unset / `auto` | stage `target/release/<lib>` if present, else build a pure wheel |
| `build` | run `cargo build --release -p omni-ffi`, then stage the result |
| `skip` | never stage — editable installs and the SDK's own tests |
| a path | stage that file or directory, after checking its name and ABI |

Before copying, the file must carry this platform's name, load through
`ctypes.CDLL`, and answer `omni_abi_version()` with the SDK's own constant, so a
wheel cannot be built around a library built for another OS or an older commit.
`OMNI_REQUIRE_NATIVE=1` turns "nothing staged" into a build failure; CI sets it
so a release job cannot quietly publish a wheel that will not load. The staged
file is declared as package data (`omni = ["_native/*"]`), because it is data to
the Python package even though it is code to the machine.

`Distribution.has_ext_modules()` returns true when an artifact will be staged,
which is what makes setuptools emit a platform-tagged wheel instead of
`py3-none-any`. The tag that comes out is `cp315-cp315-win_amd64` — true of the
build interpreter, misleading for a `ctypes` package that works on every CPython
from 3.9 — so CI retags it:

```bash
python -m wheel tags --python-tag py3 --abi-tag none --platform-tag win_amd64 \
    omniengine-0.1.0-cp315-cp315-win_amd64.whl
```

`py3` and `none` are claims the code can keep: no C extension module is
imported, the only platform fact is a shared library opened at run time. An
in-tree build with `OMNI_NATIVE=skip` still produces a pure wheel, which is what
an editable checkout install wants.

Measured on this machine with the release engine:

| | |
| --- | --- |
| `target/release/omni.dll` | 1,289,387 bytes |
| `omniengine-0.1.0-py3-none-win_amd64.whl` | 541,369 bytes, 8 files |
| contents | `omni/__init__.py`, `omni/_ffi.py`, `omni/_native/omni.dll`, LICENSE, METADATA, WHEEL, `top_level.txt`, RECORD |
| install and run outside the repo, no `OMNI_*` set | `42.0`, `packaged wheel`, batch `[2.0, 'two']`; `load_library()` resolved to `site-packages\omni\_native\omni.dll` |

**The npm tarball.** `files` is a whitelist — `index.js`, `lib/`, `native/`,
`scripts/postinstall.js`, `README.md`, `LICENSE` — so a test file or a build
leftover cannot ride along. `postinstall` loads the artifact for the current
machine through `loadBindings`, which means the ABI and nine layout facts (three
sizes, six offsets) are checked before the first script runs; it downloads nothing, and a missing or
mismatched engine is a warning unless `OMNI_STRICT_POSTINSTALL=1`. `prepack`
(which `npm pack` and `npm publish` run automatically) checks that the
workspace, the wheel metadata and `package.json` agree on the version, that the
three ABI constants agree, and that `native/` holds one engine per advertised
platform — a missing engine is a warning during a developer pack, and a failure
in `npm run pack:check`, which is the strict form CI uses.

Measured, with the Windows release engine staged:

| | |
| --- | --- |
| `npm pack --dry-run` | 8 files, 535.7 kB packed, 1.4 MB unpacked |
| contents | `LICENSE`, `README.md`, `index.js`, `lib/bindings.js`, `lib/discover.js`, `native/win32-x64/omni.dll`, `package.json`, `scripts/postinstall.js` |
| `postinstall` | `bundled engine verified (win32-x64, ABI 4)` |
| extracted tarball, koffi on `NODE_PATH` | `42 | packaged npm | [ 2, 'two' ]` |

The published tarball carries five engines, not one, so its packed size is
larger than the 535.7 kB measured here; the four non-Windows libraries cannot be
built on this machine, and no number for them is claimed. The same is true of
the wheel: one platform is measured, the other four are produced by the
workflow on their own runners.

**The C archive.** `scripts/package.py` builds one target, stages a directory,
and writes both formats plus a checksum:

```bash
python scripts/package.py                       # host target, release build
python scripts/package.py --target aarch64-apple-darwin --skip-build
```

Measured for the host target (`x86_64-pc-windows-gnu`): the zip is 538,510
bytes and the tar.gz 537,745 bytes, 6 files each — `include/omni.h`,
`lib/omni.dll` and `lib/libomni.dll.a`, `examples/c/embed.c`, `README.md`,
`LICENSE` — and `sha256sum -c` agrees with the `.sha256` sidecar. The shipped
C example was compiled against the extracted archive's header and import
library and printed `all C embedding checks passed`, so the archive is not just
a file listing.

**One workflow.** `.github/workflows/release.yml` runs on a `v*` tag (or by
hand): a guard job, then the full test suite, then five native builds — Windows
x64 MSVC, Linux x64, Linux arm64, macOS x64, macOS arm64 — each producing an
archive, a wheel, and a library for the npm tarball, then the publishes and a
GitHub release. The guard is `scripts/check-versions.py`, which reads the
version from Cargo, `pyproject.toml` and `package.json`, the ABI from `omni.h`,
`_ffi.py` and `bindings.js`, and the tag from `GITHUB_REF_NAME`; any
disagreement fails the run before an artifact exists. CI sets
`RUSTUP_TOOLCHAIN=stable` because the committed `rust-toolchain.toml` pins the
Windows GNU host this machine uses, which is not what a Linux or macOS runner
needs. Publishing uses PyPI Trusted Publishing (no stored password) and
`NPM_TOKEN`; the runbook, including what to do when a publish half-succeeds, is
`docs/RELEASING.md`.

---

## 7. Verification

One command, and it is the acceptance test for the whole brief:

```bash
./scripts/verify-all.sh
```

Measured output on 2026-11-07, with the packaging increment in it:

```
== OmniEngine cross-host verification ==
   workspace: /d/OmniEngine

Rust tests... Shared library... omni-cli... C embedding... Python SDK... Node SDK... A string is the same bytes in every host... One type error, the same text everywhere... Every surface agrees on the ABI level... String results do not grow memory... A host function answers the same bytes in every host... A host failure is data, and says so identically... Re-entering the engine from a callback is refused, not crashed... The registry says the same thing from both sides... A batch answers the same items in every host... A failing item names itself identically in every host... A batch counts items, not crossings, and reserves nothing extra... A batch inside a host callback is refused, not crashed...
== Results ==
  PASS  cargo test --workspace (225 tests)
  PASS  omni.dll (5.4M)
  PASS  omni run examples/hello.omni -> 42
  PASS  omni run examples/strings.omni -> hello, world 😀
  PASS  examples/c/embed.c -> 42 via omni.h
  PASS  python ctypes -> 42.0
  PASS  python SDK tests (107 tests)
  PASS  node koffi -> 42
  PASS  node SDK tests (ℹ tests 93 ℹ pass 93 ℹ fail 0 )
  PASS  concat + \u{...} identical in: cli python node
  PASS  the mixed-type message survives the boundary in: cli python node c
  PASS  ABI 4 declared and read identically: header=4 C=4 python=4/lib=4 node=4/lib=4
  PASS  2000 string executions, one rewind each, reservation flat: python(65536 B) node(65536 B)
  PASS  an argument crossed in and a string crossed back out, identically in: python node
  PASS  an exception was caught at the boundary and named in: python node
  PASS  a second borrow of the same handle errors and the engine lives on in: python node
  PASS  three registrations, one removal, then the name is unknown in: python node
  PASS  four items, one crossing, the same values in: python node
  PASS  statuses 0 2 0 2 and the engine text for items 1 and 3, identically in: python node
  PASS  8000 items in 1000 crossings, execs +8000, reservation flat: python(0 B) node(0 B)
  PASS  a batch from a borrowed handle errors with the shared refusal text in: python node

All checks that could run, passed.
```

`verify-all.sh` tests the four hosts against a checkout build. The release
artifacts are checked separately, because they are what a consumer gets and
nothing in the suite above touches them: a wheel installed into a clean venv
outside the repository, an extracted npm tarball, and a C archive compiled
against its own header. Those runs are recorded in section 6.6, with the sizes
they produced.

The last twelve rows are the ones that could not exist in one host. The string
comparison is byte-exact — an escape decoded differently in Rust and Node passes
both test suites and is still a broken bridge — and the ABI row reads the number
off the header, the loaded library as C sees it, and the library as each SDK sees
it. The egress row runs 2,000 string executions per host and requires the
reservation to be identical before and after. The four callback rows then check
what only a *round trip* can: that a host's answer arrives as the same bytes in
Python and in Node, that a host's exception surfaces as the same message rather
than as a crash, that a callback which re-enters its own engine is refused and the
engine still evaluates `6 * 7` afterwards, and that the registry count and the
unknown-name error agree on both sides. The four batch rows are the same idea
for `omni_exec_many`: the same four items come out of one crossing as the same
values in both hosts, a failing item reports the same statuses and the same
engine text, the counter proves 8,000 items in 1,000 crossings are 8,000
executions with a flat reservation, and a batch from inside a callback meets the
same refusal.

The script degrades to `SKIP` rather than lying. `cargo`, `gcc`, `python` and
`node` each have their own guard, Node without `koffi` is skipped with
`npm install koffi` in the reason, and each cross-host row skips itself when fewer
than two hosts answered. Nothing was skipped on this machine — all twenty-one
rows ran. `omni-wasm` has no row here because `wasm32-unknown-unknown` is not
installed; its 7 tests run under `cargo test --workspace`, where the crate
compiles for the host target and asserts its status codes against the same
constants the C header declares.

Against the brief's five phases:

| Phase | Asked for | Verified by |
| --- | --- | --- |
| 1 | Workspace on D:, fast compiles, managed `target/` | `cargo test --workspace` 225 tests; `target/` measured at 351 MB |
| 2 | Ultra-fast memory-safe core, no GC | NaN-boxed 8-byte `Value` asserted for size; arena reset is O(chunks); egress rewind keeps reservations flat over 100,000 executions; no collector exists to pause |
| 3 | C ABI callable from Python and JavaScript | same `42` and the same `hello, world 😀` from Rust, C, Python, Node; 16 exports confirmed with `objdump -p`; `omni.h` compiles warning-free under `-Wall -Wextra` |
| 4 | A custom extension GitHub recognises | `.gitattributes` tier 1; the tier-2 limit is stated in section 5.3 rather than glossed over |
| 5 | `pip`/`npm` SDKs that do not move the language stats | 107 + 93 SDK tests; `linguist-vendored` on `sdks/**`; release packages carry a CI-built engine, checkouts carry none |

Test totals: **152** in `omni-core`, **59** in `omni-ffi` (18 unit, 13 in
`tests/batch.rs`, 20 in `tests/ffi_contract.rs`, and 8 in
`tests/host_callbacks.rs`, which drive the ABI exactly as a host does), **7** in
`omni-cli`, **7** in `omni-wasm`, **107** Python, **93** Node. The batch and
callback suites are contract tests against the header, not unit tests of the
implementation they sit next to.

Static checks are part of the definition of done:

```bash
cargo clippy --workspace --all-targets -- -D warnings   # clean
cargo fmt --all --check                                  # clean
```

---

## 8. Storage ledger

Measured, all of it on this machine, at the time of writing. Sizes are `du`,
which is what the drive charges; file-byte totals differ because the registry
cache and cargo's `deps` contain hardlinks.

| What | Size | Where | Note |
| --- | --- | --- | --- |
| Source tree, tracked and to-be-tracked files | 1.1 MB | D: | 66 files, 0.94 MB of file bytes; excludes `target/`, `node_modules/`, `dist/` and `build/` |
| `target/` total | **351 MB** | D: | 21 MB under `target/release`; release builds are the expensive part |
| `target/debug/omni.dll` | 5.4 MB | D: | the artifact a checkout's hosts load |
| `target/release/omni.dll` | 1.3 MB | D: | the artifact a release package carries |
| `dist/` | 1.6 MB | D: | one wheel and two C archives, locally built |
| `CARGO_HOME` | 969 MB | C: | 897 MB registry + 170 MB `bin` by file size; none of it from this project |
| `sdks/nodejs/node_modules` | 2.9 MB | D: | koffi prebuild, gitignored |
| Free on C: | **6.5 GB** (94 % used) | | the binding constraint |
| Free on D: | 46 GB (68 % used) | | |

`cargo clean` returns the 351 MB in a second, and it is the only routine way to
do so. The knobs that keep it that size are in section 2.4: no dependencies, so
no dependency artifacts; `debug = "line-tables-only"`, so no PDBs. A release
build adds its own `target/release` tree on top — that is the 21 MB, and it is
also what a wheel or an archive is made from.

**Zero dependencies is a storage decision, not an aesthetic one.** `omni-core`
and `omni-cli` have no third-party crates, so building and testing this project
fetched nothing. That is checkable rather than asserted: walking `CARGO_HOME` by
modification time found exactly **one** file touched in the last 72 hours —
cargo's own `.global-cache`, 160 KB of metadata — and no new files. Adding
`clap`, `cffi`, or `wasm-bindgen` is a disk-cost decision here, not just a
complexity one.

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

**`speaks ABI version 2; this SDK is written for ABI version 3`** — that is the
gate working. The artifact on disk predates the increment, and because the value
layout has not changed size since version 1, reading it with a newer SDK's
assumptions would produce plausible wrong numbers instead of an error. `cargo
build -p omni-ffi`, or check you have not got two libraries and the discovery
order picked the stale one. Both
SDKs tell you which file they mean: the Python error opens with the full path of
the library it loaded (and when it finds none, it lists every location it tried),
and in Node `engine.libraryPath` is the absolute path of the artifact that engine
is talking to. `OMNI_ENGINE_LIBRARY` pins one exact file in either host, which is
the shortest way to prove the search found what you think it found.

**A string result changed after I ran another program** — expected, and the
reason the SDKs copy. The bytes live in the egress block, which rewinds at the
start of the next `omni_exec`, so an address you kept now answers with the newest
result. It is a wrong read, never a crash, and it is asserted rather than argued:
`embed.c` reads a superseded 12-byte result back as the full 27 bytes that
replaced it, and both SDK suites read `"first"` back as `a muc` — the same fact,
bounded by the stale `len` the caller passed as its cap.
Copy before the next call — `omni_str_copy`, or just `engine.string(...)` — and
the `str` or `Buffer` you get is yours for as long as you like.

**`omni_str_copy` returned 0 for a string I know is there** — the ownership
check refused the address. It is 48 bits of a NaN box the caller can write, so
the engine only copies what it can find in its own egress block, current arena,
or live host bindings. A value from another engine, a host string that has since
been redefined, or a `OmniValue` you filled by hand all copy zero bytes rather
than reading memory they have no claim on. Passing `cap == 0` or a NULL buffer
does the same, on purpose.

**A host function got `OMNI_ERR_REENTRANT`, or read garbage arguments** — a
callback may not call back into the engine that called it, and the ABI answers 6
rather than creating a second mutable reference. The two ways to hit it are
deliberate in the test suites: calling `omni_exec` from inside a callback, and
calling `omni_str_copy` on one of your own *arguments* — that helper is an engine
call, and an argument's `payload.ptr` is already the text, so `memcpy` / `bytes` /
`Buffer.from` is the read. If a host genuinely needs the engine's answer, compute
it before the call and pass it in, or register it as a second function.

**A callback worked in one host and not another** — check the arity first. The
count is fixed at registration and the engine validates it before entering foreign
code, so a Python callable registered with `arity=2` and called as `f(1)` is a
runtime error naming both counts, never a partially-filled argument list.

**Node aborts the process with `FATAL ERROR: Error::ThrowAsJavaScriptException`
and no stack** — a koffi signature named a type koffi had not been told about. It
is not a catchable `Error`; the process dies. The opaque `OmniEngine *` is exactly
the type that triggers it, which is why `lib/bindings.js` declares
`OmniPayload` → `OmniValue` → `OmniStats` in that order and keeps the handle an
unregistered `void *` — `const void *` only in `omni_str_copy`, the one export
whose engine argument is const, and for the two input buffers `omni_exec` and
`omni_define_string` take. A *missing symbol* is different and harmless: koffi
throws a normal `Error` at declaration time, which is how the ABI check finds an
old library that has no `omni_abi_version`.

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

## 10. What comes next

Three increments shipped after the first bridge. Strings made `OmniValue` a union,
promoted a result's bytes into a block a host can copy from, and made
`omni_str_copy` refuse addresses it cannot prove. Host callbacks then made the
boundary bidirectional: a script can call foreign code, the engine borrows its own
state through `RefCell` so a re-entrant call is an error rather than an alias, and
every host turns an exception into a message instead of an unwind. Batch execution
then made the boundary carry several programs at once, and the three benchmark
harnesses (4.9) turned every performance claim in this file into a quoted run.
A fourth round packaged what existed: wheels and an npm tarball that carry the
engine, a C archive per target, one tag-driven workflow, and a quickstart that
fits in a minute. Nothing in that round changed the language or the ABI.

Release-side, three things are deliberately unfinished rather than unknown:

- **0.1.0 is packaged, not published.** The workflow builds every artifact and
  the local runs in section 6.6 passed, but PyPI and npm have not been touched.
  Publishing is a tag and two credentials (`docs/RELEASING.md`).
- **Linux wheels are `manylinux_2_39`.** They are built on Ubuntu 24.04 because
  that is the runner with the newest glibc, which makes them work on 24.04 and
  later and *not* on 22.04. Lowering that floor means an older container, not a
  code change.
- **Windows arm64 is not in the matrix.** Five targets ship; that one has no
  runner in the workflow and no artifact claimed for it.

What the same memory model is still waiting for, in rough order:

1. **Booleans, `nil`, `undefined` as literals.** The tags and singletons already
   exist in `Value`; only the parser is missing. Booleans are values today — you
   can get one from `==` — but you cannot write one.
2. **String operations.** `length`, indexing, slicing, comparison order, and a
   way to get a number or a bool *into* a string. The box already stores the
   length, so this is evaluator work, not layout work.
3. **A zero-copy result path for a host that wants one.** The harnesses in 4.9
   say the crossing is 36–240 ns and the wrapper is microseconds, and the
   wrapper's biggest single cost is copying string bytes out. A borrow API —
   pointer valid until the next execution, on the same rule 1 a host already
   lives under — is the next performance increment, and the benches are the
   instrument it will be judged by. A batch already makes all N results live at
   once, so a borrow and a batch compose.
4. **An AST.** Extracting one from the current one-pass evaluator, so functions
   and control flow have something to be built from. A call node is the first
   thing an AST would buy back: today `parse_call` evaluates its arguments as it
   reads them, so a host function cannot see the source text of its own call, and
   that is what macros, lazy arguments, and closures would need.
5. **User-defined functions and control flow.** Host functions settled the call
   *syntax*, the arity check, and the depth guard, so what remains is language-side
   frames — which is where the arena's `reset`-per-exec model needs re-examining.
6. **wasm for real.** `wasm-bindgen`, the `wasm32-unknown-unknown` target, and a
   browser example; the shape is already scaffolded and asserted to agree with
   the C statuses. A wasm string will need its own copy rule, since there is no
   shared address space to rewind inside, and a host callback needs a `JsFunction`
   rather than a function pointer.
7. **Linguist tier 2.** The upstream PR from section 5.3, which would also make
   the new `examples/strings.omni` show up under its own language name.

**One cosmetic defect, deliberately left for its own commit.** `display_kind`
already quotes a symbol — `'='` — and two parse-error templates wrap it in quotes
again, so `let = 1` reports `found ''=''`. It is in `crates/omni-core/src/engine.rs`
at the `format!` calls in `expect_ident` and `parse_error_kind`, and no test
asserts the doubled form, so it is a safe first issue. It stayed out of this
increment because the strings work changed the ABI and the memory model, and a
commit that does both is a commit whose regressions cannot be attributed. The SDKs
quote the doubled text in their READMEs so that what is documented is what runs
until the fix lands.

Deferred on purpose, not forgotten: no GC (there is nothing to collect), no
incremental compilation tricks beyond `incremental = true`, no dependency
graph to cache.
