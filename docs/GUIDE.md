# OmniEngine — initialization and execution guide

Everything below was run on this machine, and every number is measured rather
than estimated. Where the design and the reality diverged, the reality is what
is written here, with the reason.

Original brief: a high-performance custom language and ecosystem in `D:\OmniEngine`,
lightweight and modular, with a Rust workspace, a C ABI that Python and
JavaScript can call, a custom file extension that GitHub recognises, and
SDKs that install through `pip` and `npm`.

Two numbering systems are in use and both are kept, because the brief's five
phases are how the work was asked for and how section 11 is scored. **"Phase 1–5"
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
| Phase 2 — core engine | done | 176 `omni-core` tests |
| Phase 3 — C ABI + header | done | 62 `omni-ffi` tests, PE export table, `examples/c/embed.c` |
| Phase 4 — extension and Linguist | done (tier 1) | `.gitattributes`, `examples/*.omni` |
| Phase 5 — SDKs and DX | done | 131 Python + 126 Node tests, CLI, wasm scaffold |
| Strings, egress memory, ABI v2 | done | 4 new exports, `omni_str_copy` + `omni_define_string` + `omni_debug_stats` + `omni_abi_version` tested from all four hosts |
| Host callbacks, re-entrancy, ABI v3 | done | `omni_register_host_fn` + `omni_unregister_host_fn` + `omni_host_fn_count`, a `RefCell` borrow guard on every entry point, and call syntax in the parser |
| Batch execution, benchmarks, ABI v4 | done | `omni_exec_many` + `omni_batch_error`, checkpointed arena reclamation, three zero-dependency benchmark harnesses and `scripts/bench.sh` (§4.8, §4.9) |
| Packaging, distribution, release | done | platform wheels with the engine inside, an npm tarball with five engines, `scripts/package.py`, `scripts/check-versions.py`, `.github/workflows/release.yml` (§6.7, `docs/RELEASING.md`) |
| DevPack: builtins, positions, timer, sugar | done | 17 builtins, `[Line N, Col M]` errors, `last_exec_micros` in ABI-4 padding, `eval_str` / `eval` (§3.5, §4.4, §6.1–6.2) |
| Phase 6 — bytecode VM, modules, sandbox | done | AST lowering and a stack VM behind the unchanged ABI 4, `import` with namespace isolation, `fs` / `json` builtins (21 total), 226 `omni-core` tests (§7) |
| Phase 7 — hardened security and fuzzing | done | shared execution gas, a 65,536-value VM stack cap, a 128-level parser nesting cap, symlink-safe writes, cargo-fuzz targets plus a deterministic in-repo harness, 233 `omni-core` tests (§8) |
| Phase 8 — concurrency and parallel execution | done | `Send + Sync` core, `Engine::fork`, engine-local module caches, `omni_exec_parallel`, 64-thread stress suite, `run_parallel` / `runParallel`, 243 `omni-core` tests (§9) |
| Phase 9 — optimizer, profiler, diagnostics | done | constant folding + DCE ahead of the VM, `OmniProfile` heatmaps through three additive ABI-4 exports, 261 `omni-core` tests (§10) |
| Auto-updater and CLI manager | done | `omni_check_update` in the C ABI, `omni update` / `omni check-update` / `omniup`, `omni.check_updates()` and `checkUpdates()`, `scripts/cli-binaries.py` + `scripts/make-manifest.py` (§6.4–6.5) |
| Same answer from four hosts | done | `scripts/verify-all.sh` — 37 rows, all passing |

```
cargo test --workspace          # 373 tests
./scripts/verify-all.sh         # Rust, C, Python, Node: 42, "hello, world", ABI 4
./scripts/fuzz.sh               # libFuzzer when nightly is present, harness otherwise
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

### 3.4 The grammar, and how a program is executed

A program is lexed into a token buffer, parsed into a flat AST, lowered into
bytecode, and executed by a stack VM (section 7). The AST is deliberately flat:
a node is an index into one `Vec<Node>`, every child is a `u32`, and `NONE` is
the absent child. The parser never calls a `Node` by value and lowering never
recurses, so expression depth costs heap that the engine already owns rather
than native stack.

```
program  := ( stmt )*                      // ';' is an optional separator
stmt     := 'let' IDENT '=' expr | 'import' STRING | expr
expr     := additive ( ( '==' | '!=' ) additive )*
additive := term ( ( '+' | '-' ) term )*
term     := factor ( ( '*' | '/' | '%' ) factor )*
factor   := ( '-' | '+' )? primary
primary  := NUMBER | STRING | call | qualified | '(' expr ')'
call     := qualified '(' ( expr ( ',' expr )* )? ')'  // <= MAX_ARGS arguments
qualified := IDENT ( '.' IDENT )?
```

The `IDENT '('` lookahead is what separates a call from a name: `pair` reads a
binding and `pair()` reads the host function registry, so a host may install both
under one name and the grammar tells them apart without a second namespace. A
dotted name is a module export (`math.answer`) and a dotted call is checked
against the registry exactly like a bare one. The call rule sits at the
*primary* level, which is why a call binds tighter than every
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
- Every error carries a 1-based line and byte column, and both survive the FFI
  boundary (section 4.4).

### 3.5 Builtins, positions, and the execution timer

Twenty-one builtins are callable from any script with the same syntax a host
function uses. A host registration with the same name wins, and arity is
checked before the call is entered, so a builtin can never see a call it was
not written for.

| Builtins | Arity | Semantics |
| --- | --- | --- |
| `abs`, `floor`, `ceil`, `round` | 1 | operate on `f64`; `round` is half away from zero |
| `sqrt(x)` | 1 | `f64`; a negative argument is a runtime error, not `NaN` |
| `min`, `max` | 2 | `f64` |
| `pow(base, exp)` | 2 | `f64`; a result that is not finite is a runtime error, not `inf` |
| `clamp(value, low, high)` | 3 | `f64`; `low > high` is a runtime error rather than a silent empty range |
| `len(text)` | 1 | **byte** length of the UTF-8 payload, not a character count |
| `trim`, `upper`, `lower` | 1 | Unicode-aware, so `upper("straße")` is `"STRASSE"` |
| `contains(haystack, needle)` | 2 | boolean; the empty needle is in every string |
| `replace(text, old, new)` | 3 | leftmost-first; an empty `old` is a no-op, not an insertion between characters |
| `slice(text, start, end)` | 3 | **character** offsets, clamped to the string; an inverted range is empty |
| `type_of(value)` | 1 | `"number"`, `"string"`, `"boolean"`, or `"null"` |
| `fs.read_text(path)` | 1 | UTF-8 text from a file inside a granted read root; at most `MAX_STRING_LEN` |
| `fs.write_text(path, text)` | 2 | writes UTF-8 bytes inside a granted write root; returns `null` |
| `json.parse(text)` | 1 | one scalar: number, string, boolean, or `null` |
| `json.stringify(value)` | 1 | JSON text for a scalar; `undefined` is a runtime error |

`slice` deliberately counts characters while `len` counts bytes: a byte cut can
land inside a multi-byte character and produce text no host can decode, so the
cutting operation pays for a character walk and the measuring operation keeps
reporting the storage a caller allocates for. `type_of` answers `"null"` only
for a value a host callback produced, because the grammar has no `null` literal
yet; there is no `"array"` or `"object"` because ABI 4 has no collection type.

The `fs` and `json` builtins are sandboxed. No permission root exists by
default, so both filesystem calls fail closed with
`fs.read_text is disabled: this engine has no filesystem read permission`. A
Rust embedder opts in with `Engine::allow_fs_read(path)` or
`Engine::allow_fs_write(path)`, which canonicalize the directory and return an
`io::Result`. A read canonicalizes the target and requires it under a read
root; a write canonicalizes an existing target (including a symlink) and
refuses one that resolves nowhere, while a genuinely new path is checked
through its canonical parent, so `..` cannot escape. A target outside every
root is the engine's
`path '…' is outside the allowed read roots`, not the platform's wording, so
the same bytes reach every host. JSON covers the four scalar kinds a 16-byte
value can carry; an array or object is a deliberate error rather than a
string-shaped stand-in, and `json.stringify(undefined)` is refused because
dropping it would produce text that parses back as a different program's value.
Stringification escapes quotes, backslashes, and control characters and passes
non-ASCII UTF-8 through.

The string builtins reuse one scratch buffer on the engine, then copy the result
into the egress block: a chain such as `upper(trim(s))` builds no temporary
string per call. The buffer is cleared before each use, so an editing builtin
can never inherit the previous call's prefix. A `len` of `"héllo"` is 6, and
that is deliberate — the language's only string unit so far is a byte slice, and
a character count would have to be defined against grapheme clusters to be
stable.

Every error now carries a position: a 1-based line and a 1-based **byte**
column, rendered as `[Line 2, Col 14]: message`. The column counts bytes because
that is the unit the lexer already stores in every token, so `"é" + 1` puts the
operator at column 6 rather than pretending the `é` is one byte. Position
selection is part of the contract: a binary operator error points at the
operator, a builtin type or arity error points at the call name, an undefined
variable points at the identifier, and an unexpected end of input points at the
end of the source. A wrong argument count names the call:
`[Line 1, Col 1]: Mismatched arity in call 'clamp': takes 3 arguments, got 2`.
The text is rendered by the engine, so the same bytes reach every host (one
`verify-all.sh` row asserts it).

`stats()` carries `last_exec_micros` (`lastExecMicros` in Node): the wall-clock
time of the most recent `exec` or `exec_many` call in microseconds, saturating
at `u32::MAX` rather than wrapping. The field lives in four bytes that were
padding at offset 4 of `OmniStats`, so the other offsets and the 72-byte size
are unchanged and the ABI remains 4.

### 3.6 Strings: three blocks, three lifetimes

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
[Line 1, Col 6]: type mismatch: cannot add string to number: "hi" + 42
```

`==` and `!=` compare content, not addresses. There is no implicit coercion in
either direction: `42 + "hi"` is `cannot add number to string: 42 + "hi"`, and a
boolean reaches the same arm as `cannot add boolean to string: false + "hi"` — all
three messages asserted verbatim in `engine.rs`. `verify-all.sh` then checks that
the `"hi" + 42` line arrives in Rust, C, Python, and Node with identical bytes.

The counting allocator turns "no GC" into something a host can measure rather
than a claim. `omni_debug_stats` fills a `#[repr(C)]` struct with `execs`,
`last_exec_micros`, `strings`, arena in-use/reserved/chunks, and egress
in-use/reserved/high-water. Measured here, 5,000 consecutive string executions of
`"hello, " + "world" + "\u{1F600}"` (30,000 strings built):

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

### 3.7 What the core still does not have

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
host may point at is promoted into the egress block first (section 3.6), which is
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
| `omni_debug_stats(e, out)` | status; fills `OmniStats` (counters plus `last_exec_micros`), whose `size` field says which layout was written |
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
const _: () = assert!(std::mem::offset_of!(OmniStats, last_exec_micros) == 4);
```

with `_Static_assert` in `omni.h`, ten `ctypes` offsets and sizes checked at
Python import, and ten `koffi` offsets and sizes checked at Node load. Four
independent assertions of the same layout is the point: the Rust one catches the
crate drifting, the C one catches the header, and the two SDK ones catch a host
that was installed from an older tag.

`OmniValue` is `{ u32 tag, u32 len, union { double number; const char *ptr } }`.
The union was the spec's shape and it is now the real one: `ptr` for `STRING`, with
`len` as its byte count, and `number` for everything else — the double itself for
`NUMBER`, `1.0` or `0.0` for `BOOL`, `0.0` for `NIL` and `UNDEF`. A non-string
always has `len` 0, and no arm is ever left holding raw engine bits that could be
mistaken for a value: the tag alone says which of the two to read.

### 4.4 Five ABI details that only showed up under testing

**`omni_last_error` copies the rendered error, not just the message.** The first
version copied `error.message`, so a host received `expected a name` with no line
and no way to point at the offending token. It now copies the `Display` form
whole — `[Line 1, Col 5]: expected a name, found ''=''` — and the contract test
asserts both the line and the column survive the boundary. Note the doubled
quotes around `=`: that is the engine, not the SDK. `display_kind` renders a
symbol as `'='` and the message template wraps it in quotes a second time. The
hosts pass the text through untouched rather than prettifying it, and section 14
records the one-character fix.

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

**The execution timer fits in padding, so ABI 4 did not move.** `OmniStats` had
four bytes of padding between `size` and the first `u64`, because an eight-byte
counter must start at offset 8. `last_exec_micros` is a `u32`, so it occupies
exactly those bytes: every existing offset, the 72-byte size, and the ABI
version are unchanged. The layout assertions in Rust, C, Python, and Node all
pin offset 4, so a compiler that reordered the struct would fail four tests
instead of silently misreading the timer. Saturation at `u32::MAX` (about 71
minutes) is deliberate — a wrapping timer would report a fast batch after a slow
one.

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

One handle, one borrow at a time. `&mut Engine` is what the ABI assumes
internally, and every export opens a `RefCell`, so the SDKs document the rule
instead of pretending to enforce it. Phase 8 refines the picture in two ways: the
Rust `Engine` is `Send + Sync` and forkable, and `omni_exec_parallel` spreads one
batch over forked worker engines while the handle itself stays borrowed (§9).

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
answer `OMNI_HOST_ERR` — `[Line 1, Col 1]: host function 'boom' failed: ValueError:
host said no`. The host's own stack is gone by then, so the text is all either side keeps,
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
looks, and section 6.7 describes the packaging itself.

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
    print(engine.eval_str("let answer = 40 + 2"))          # 42, a Python int
    print(engine.eval_str('1 == 1'))                       # True
    print(engine.eval('upper(trim("  hi  "))'))            # HI
    print(engine.eval('type_of(2)'))                       # number
    print(engine.stats().egress_bytes_reserved)            # 65536
    print(engine.stats().last_exec_micros > 0)             # True
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
returns a `float` and raises `OmniError` on any non-number or failure;
`eval_str` returns the language value as a native Python object — an integral
finite number becomes `int`, everything else stays `float`, `str`, `bool`, or
`None` — and raises only when the program itself fails; `eval` is that same
unwrapping under the name Node already used, so a program can be pasted between
the two SDKs; `string` returns a `str` and raises on any non-string; `try_run`
returns `(status, result | None, message)` and never raises. `close()` is
idempotent and `__del__` is guarded by an `atexit` hook, so an engine that
outlives the script is not a use-after-free.

Neither `eval_str` nor `eval` produces a `list` or `dict`: ABI 4 has no
collection type for a program to return, so converting one would be inventing a
value the engine never made. When collections land, the conversion belongs in
`Result.value`, which both names read.

`eval_str` folds integral floats to `int` because the alternative hands a caller
`42.0` for a value it will print, index, or compare as an integer. The fold only
happens for finite integral values, and it is a wrapper decision — `run().value`
still reports the engine's raw `float`, and `Result.number` is unchanged.

`Result.text` is the exact `bytes` of a string result — a NUL inside a string is
data, not a terminator — and `Result.value` is those bytes decoded as UTF-8.
`Result.number` is `0.0` for a string on purpose: those eight payload bytes are
an engine address, and `float`-printing one would be noise in a log. The copy
happens inside `run`, before anything can rewind the block it points into; the
SDK never hands out an address, so there is nothing for a caller to hold past its
lifetime.

`_ffi.py` checks ten `ctypes` sizes and offsets at import and compares
`omni_abi_version()` against its own constant, so an engine built from an older
commit raises at import with `cargo build -p omni-ffi` in the message instead of
returning numbers read from the wrong offsets.

The dependency list in `pyproject.toml` is empty and that is load-bearing: the
binding is `ctypes`. `cffi`, `pybind11`, and a build-time compiler would each add
an install failure mode to a package whose entire job is to find one DLL.

The license is VGPIL 1.0, which is not an SPDX identifier, so the metadata
declares no `License-Expression` at all and ships the text under PEP 639
`license-files`; `setuptools>=77` is what understands that field. `twine check`
accepts a wheel with `License-File` and no expression. Cargo has the same rule
from the other side: the workspace uses `license-file = "LICENSE"` instead of a
`license` string, and the crates inherit it.

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
console.log(engine.eval('let answer = 40 + 2'));      // 42, a JS number
console.log(engine.eval('1 == 1'));                   // true
console.log(engine.eval('upper(trim("  hi  "))'));    // HI
console.log(engine.stats().egressBytesReserved);       // 65536
console.log(engine.stats().lastExecMicros > 0);        // true
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
// [Line 1, Col 1]: host function 'config' failed: a host function must return a number,
//         bool, string, Buffer or null, not object
engine.close();                                        // releases both trampolines
```

The last call is the point of the example: an object is not a value the ABI can
carry, so the SDK reports the shape rule as a host error the script reads with
status `5` (`host`), instead of coercing `{}` to a number and letting the program
continue on a lie.

`run` → `Result` (`tag`, `number`, `text`, `value`, where `text` is a `Buffer`);
`evaluate` → number; `eval` → the language value as a native JS primitive
(`number`, `string`, `boolean`, or `null` for an empty program); `string` →
string; `tryRun` → `{ status, statusName, result, message }` with no throw;
`stats` → plain JS numbers, never BigInt. `Symbol.dispose`
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

The CLI installs globally from the checkout; both binaries land in `~/.cargo/bin`
(already on `PATH` for anyone using cargo):

```bash
cargo install --path crates/omni-cli   # omni and omniup
omni --version                         # omni 0.1.0 (language v2, ABI 4)
omni run examples/hello.omni           # 42
```

Running it without installing works too:

```bash
cargo run -p omni-cli -- run examples/hello.omni      # 42
cargo run -p omni-cli -- run examples/strings.omni    # hello, world 😀
cargo run -p omni-cli -- eval "2 + 3 * 4"             # 14
cargo run -p omni-cli -- --version                    # omni 0.1.0 (language v2, ABI 4)
cargo run -p omni-cli -- check-update                 # 0, or 10 when newer exists
cargo run -p omni-cli -- update --dry-run             # 10, downloads nothing
```

`update` and `check-update` also answer to `--update` and `--check-update`, so a
flag-style invocation from a script works without a subcommand.

Exit codes separate the six things a script needs to tell apart:

| Code | Meaning |
| --- | --- |
| 0 | success, or already up to date |
| 1 | the program parsed but failed to run |
| 2 | usage error, unreadable file, or a failed check |
| 3 | the update failed verification or could not be installed |
| 10 | a newer release exists (`check-update`, or `update --dry-run`) |
| 11 | the release speaks a different ABI |

Argument parsing is hand-rolled; `clap` would add a dozen crates and a few
hundred MB of build cache for six subcommands.

A string result prints as its text, through `Engine::render` rather than
`Value`'s own `Display` — which deliberately prints `<string>` for a box it
cannot prove is live, because following an address is something only the engine
that owns the bytes may do. The CLI is exactly where a program's string result
should be readable, so it asks the owner. That distinction is a test
(`a_string_result_prints_its_content`), not a comment.

`--version` answers without loading a program or touching the network: it reads
the compiled-in `CARGO_PKG_VERSION`, says `language v2`, and reports the ABI the
binary was built against. A script that gates on the engine's version pays for
one process spawn, nothing else.

### 6.5 Staying current: the updater

The check and the transport are separate on purpose. The engine parses and
compares; it never opens a socket. `omni_check_update` takes a manifest **the
caller already fetched** — bytes and a length — and returns a status:

```c
int omni_check_update(const void *manifest, size_t len,
                      char *out_version, size_t out_cap);
```

| Return | Meaning |
| --- | --- |
| 0 | this build is up to date |
| 1 | a newer version is offered |
| 2 | the release speaks a different ABI |
| 3 | the manifest is not valid |
| -1 | bad argument (null pointer, oversized input, invalid UTF-8) |

It also writes the manifest's version, NUL-terminated, into the caller's buffer,
so a host can print "0.2.0 is available" without parsing JSON twice. Input is
capped at 1 MiB, the manifest must be a flat object with a version, and any
`assets` entries are validated, not interpreted. That makes one small export
usable by the CLI and both SDKs from the same bytes, and it keeps the engine
zero-dependency — no TLS, no HTTP, no JSON library. The wasm scaffold does not
expose it yet; it will mirror the same status codes when it gains a surface.

The manifest is one document per release:

```json
{
  "version": "0.2.0",
  "abi": 4,
  "notes": "what changed",
  "assets": {
    "win32-x64":    { "url": "https://…/omni-win32-x64.exe",   "sha256": "…64 hex…" },
    "linux-x64":    { "url": "https://…/omni-linux-x64",       "sha256": "…" },
    "linux-arm64":  { "url": "https://…/omni-linux-arm64",     "sha256": "…" },
    "darwin-x64":   { "url": "https://…/omni-darwin-x64",      "sha256": "…" },
    "darwin-arm64": { "url": "https://…/omni-darwin-arm64",    "sha256": "…" }
  }
}
```

**`omni update`** fetches it (default
`https://github.com/fahim5536/omniengine/releases/latest/download/omni-update.json`,
overridable with `--manifest` or `OMNI_UPDATE_URL`; `file://` is accepted for
local testing), picks the asset for this platform, downloads it, and checks the
SHA-256 before anything touches the disk. Plain `http://` is refused unless
`OMNI_ALLOW_INSECURE=1` is set, because an unauthenticated manifest is an
unauthenticated binary. Fetching uses `curl` with `--proto`/`--proto-redir`
restrictions and a 60-second cap, falling back to `wget` only when curl is
absent.

Replacement is atomic and reversible:

1. the verified bytes go to `.<name>.new` beside the target;
2. the target is renamed to `<name>.bak`;
3. the new file is renamed into place;
4. if step 3 fails, the backup is renamed back and the error is reported.

On Windows a running image cannot be deleted but can be renamed, so a binary
updating itself leaves `omni.exe.bak` and says so. The backup is the undo:
`mv omni.exe.bak omni.exe` restores the old build. Without `-y` / `--yes` it
asks before replacing, and a closed stdin — a cron job or a CI step — reads as
"no", not as consent. `--dry-run` stops after the check with exit code 10.

**`omniup`** is the same updater as a separate binary. The point is to update an
`omni` that is *not* the process doing the updating, which is what a package
manager or a self-updating install needs:

```bash
omniup check                                  # 0 up to date, 10 newer, 11 other ABI
omniup update --target ./omni -y              # replace a specific file
omniup update --manifest file:///D:/dist/omni-update.json --dry-run
```

**In the SDKs**, the check is opt-in and notify-only:

```python
import omni
result = omni.check_updates()                 # status, current, latest, abi, notes, asset
result.available                              # True when latest > current
```

```js
const { checkUpdates } = require('omniengine');
const result = await checkUpdates();          // { status, current, latest, abi, notes, asset }
```

Neither downloads or installs anything — an SDK that silently replaced the
library under a running interpreter would be a footgun, and the ABI check
already refuses an incompatible engine at load time. A background check is
available but off by default: `Engine(background_check=True)` in Python,
`new Engine({ updateCheck: true })` in Node, or `OMNI_UPDATE_CHECK=1` in either.
It runs on a daemon thread / an unawaited promise, never delays startup, never
throws, and never prints — the result is only there if the host looks.

A library built before this export existed is still usable: Python guards the
declaration with `hasattr`, Node with a `try` around `lib.func`, and a missing
symbol becomes one actionable error ("rebuild it: cargo build -p omni-ffi")
rather than a crash.

### 6.6 The wasm scaffold

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

### 6.7 Packaging: what a release contains

Four artifacts, and each one carries the engine it needs:

| Artifact | Shape | Built by |
| --- | --- | --- |
| Python wheel | one per platform, `omni/_native/<lib>` inside | `setup.py` + `build`, retagged |
| npm tarball | one for all platforms, `native/<platform>-<arch>/<lib>` ×5 | `npm pack`, `prepack` guard |
| C archive | one per target, `lib/`, `include/`, `examples/c/`, README, LICENSE | `scripts/package.py` |
| CLI binaries | one `omni` and one `omniup` per platform, plus `omni-update.json` | `scripts/cli-binaries.py`, `scripts/make-manifest.py` |

The two command-line binaries are staged with a `.sha256` sidecar under
`dist/binaries/`, named `omni-<platform>[.exe]` and `omniup-<platform>[.exe]`.
`scripts/make-manifest.py` then reads the version from `Cargo.toml` and the ABI
from `omni.h`, recomputes each adjacent digest, and writes the single
`omni-update.json` that `omni update` fetches:

```bash
python scripts/cli-binaries.py --platform win32-x64
python scripts/make-manifest.py --tag v0.1.0 --notes "what changed"
```

`--tag` must name the workspace version; on a manual run `--base <url>` builds
asset URLs instead. A missing platform is an error unless `--allow-partial` is
given, so a release cannot quietly ship a manifest that offers Windows an update
and leaves macOS behind. The digests in the manifest are the digests recomputed
here — a stale sidecar fails the manifest build, not the user's update.

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
| `omniengine-0.1.0-py3-none-win_amd64.whl` | 541,892 bytes, 8 files |
| contents | `omni/__init__.py`, `omni/_ffi.py`, `omni/_native/omni.dll`, LICENSE, METADATA, WHEEL, `top_level.txt`, RECORD |
| install and run outside the repo, no `OMNI_*` set | `42.0`, `packaged wheel`, batch `[2.0, 'two']`; `load_library()` resolved to `site-packages\omni\_native\omni.dll` |
| `twine check` | PASSED — the metadata PyPI will render, including the README and license file |

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
| `npm pack --dry-run` | 8 files, 536.5 kB packed, 1.4 MB unpacked |
| contents | `LICENSE`, `README.md`, `index.js`, `lib/bindings.js`, `lib/discover.js`, `native/win32-x64/omni.dll`, `package.json`, `scripts/postinstall.js` |
| `postinstall` | `bundled engine verified (win32-x64, ABI 4)` |
| extracted tarball, koffi on `NODE_PATH` | `42 | packaged npm | [ 2, 'two' ]` |

The published tarball carries five engines, not one, so its packed size is
larger than the 536.5 kB measured here; the four non-Windows libraries cannot be
built on this machine, and no number for them is claimed. The same is true of
the wheel: one platform is measured, the other four are produced by the
workflow on their own runners.

**The C archive.** `scripts/package.py` builds one target, stages a directory,
and writes both formats plus a checksum:

```bash
python scripts/package.py                       # host target, release build
python scripts/package.py --target aarch64-apple-darwin --skip-build
```

Measured for the host target (`x86_64-pc-windows-gnu`): the zip is 539,357
bytes and the tar.gz 538,422 bytes, 6 files each — `include/omni.h` (which now
carries the VGPIL copyright line), `lib/omni.dll` and `lib/libomni.dll.a`,
`examples/c/embed.c`, `README.md`, `LICENSE` — and `sha256sum -c` agrees with
the `.sha256` sidecar. The shipped
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

## 7. Phase 6 — Bytecode VM, modules, and the sandbox

### 7.1 Four stages behind the same four exports

Phase 6 replaced the fused parse-and-evaluate walk with four stages:

```
source -> lexer -> flat AST -> lowering -> Chunk -> stack VM
```

Nothing above the core changed. `omni_exec`, `omni_exec_many`,
`omni_register_host_fn`, and the 16-byte `OmniValue` are the same ABI 4
functions with the same bytes on the wire, because the VM is an implementation
of `Engine::exec` rather than a new entry point. What did change is the
language level: `import` is now a keyword, so `LANGUAGE_VERSION` is 2 and
`omni --version` prints `language v2`. A script that used `import` as an
identifier is the only kind that sees a difference.

### 7.2 The instruction set

The VM is a stack machine. An instruction is fixed-size and carries its own
source position, so a runtime failure points at the operator or name that
caused it without a side table.

| OpCode | Operands | Effect | Lowered from |
| --- | --- | --- | --- |
| `PUSH_NUMBER` | source span | push a number literal | a number |
| `PUSH_STR` | source span | decode and push a string literal | a string |
| `PUSH_NIL` | — | push `null` | reserved for a future `nil` literal |
| `PUSH_UNDEF` | — | push `undefined` | an empty program |
| `LOAD` | name offset, length | push a binding | `x`, `ns.x` |
| `STORE` | name offset, length | pop into a binding, push it back | `let x = e` |
| `POP` | — | discard the top | a non-final statement |
| `ADD` … `MOD` | — | pop two, push one | `+ - * / %` |
| `EQ`, `NE` | — | pop two, push a boolean | `==`, `!=` |
| `NEG` | — | pop one, push its negation | unary `-` |
| `JUMP` | target | continue at `target` | reserved for control flow |
| `JUMP_IF_FALSE` | target | pop and branch when falsy | reserved for control flow |
| `CALL` | name offset, length, argc | pop `argc` values, push one result | `f(a, b)` |
| `RET` | — | return the top of the stack | end of every chunk |
| `IMPORT` | source span | load a module, bind its namespace | `import "m"` |

`Jump`, `JumpIfFalse`, and `PushNil` have no lowering source yet — the grammar
has no `if` and no `nil` literal — but they are part of the VM contract and the
dispatch loop, and `bytecode.rs` exercises them with hand-built chunks so the
control-flow path is tested before the syntax that emits it exists. A chunk
that would underflow its stack is a runtime fault with a position, not a panic.

### 7.3 Lowering

Each statement is lowered post-order, and the only piece of native recursion is
the descent the parser already did. The compiler walks an explicit `Task` stack
owned by the engine:

- `Visit(node, depth)` pushes the work for one node. Children go on in reverse
  execution order, so the leftmost operand pops first; the combining
  instruction sits underneath them and emits only once they are done.
- `Emit(op, …)` appends the instruction.

The rules are one line each: `let e` lowers `e` then `STORE` (the value stays,
so a `let` evaluates to its binding); a binary node lowers both operands then
its opcode; a call interns its name and then schedules `CALL` under its
argument visits. Names live in `Chunk::names` and an instruction addresses a
byte slice, so `LOAD`/`STORE`/`CALL` stay fixed-size and a warm program allocates
no `String` per identifier. String literals stay as source spans and are decoded
when `PUSH_STR` runs, which is what keeps `1 / 0` reported before a bad escape
later in the same expression.

The order of the three call checks is the fused parser's, because it is
observable: depth (`calls are nested 64 deep…`), then existence
(`unknown function 'f' …`), then the arguments. `nope(1 / 0)` must name `nope`.
Depth is syntactic — the number of enclosing calls — so `id(id(…))` hits the
same limit at the same nesting it always did, without growing the VM stack.

The buffers — tokens, AST, chunk, VM stack, and the lowering work stack — are
all engine-owned and cleared rather than freed. A 100,000-term `1 + 1 + …`
lowers and runs without touching the native stack; the test lives in
`engine.rs`, and the Python SDK's 2,000-term row runs through the same path.

### 7.4 Modules

Language version 2 adds one statement, `import "path"`. The request resolves to
`<module_root>/<path>[.omni]`, is canonicalized, and must start with the
canonical module root; absolute paths, `..`, and symlinks that leave the root are
refused by that one check. The namespace is the file stem, so `lib/math` and
`lib/math.omni` both bind `math`.

Each module runs in a **fresh child engine**. It sees builtins, including `fs`
and `json`; it inherits the parent's filesystem roots and module root; it does
**not** inherit the parent's globals or host-function registrations, so a module
cannot call a callback the embedder never exposed to it. After it runs, every
top-level `let` whose name has no dot and does not start with `_` is exported —
the leading underscore is the entire privacy rule. Imported names are dotted and
are not re-exported. Values are copied into the importing execution: scalars by
value, strings by copying the bytes out of the child arena before it is
reclaimed.

There are no function values at language version 2, so a module exports data,
not behaviour. `math.answer` is a `LOAD`; `math.answer()` fails with the ordinary
unknown-function error naming `math.answer`. Import depth is capped at 16
(`MAX_MODULE_DEPTH`); a cycle is detected against the canonical paths currently
loading and reported as `circular import: a -> b -> a`; an error inside a module
keeps its line and column and gains the prefix `in module 'math': …`. There is
no module cache in Phase 6 — importing the same file twice executes it twice,
which keeps exported strings owned by the execution that used them. Permission
is opt-in: `Engine::set_module_root` must be called, or imports fail with
`import is disabled: this engine has no module root`.

### 7.5 The filesystem and JSON builtins

`fs.read_text` and `fs.write_text` are described with the rest of the toolkit in
§3.5: no root means fail closed, canonicalization decides containment, and the
refusal is the engine's text rather than the platform's. `json.parse` and
`json.stringify` handle exactly the four scalar kinds a 16-byte value can carry,
and refuse arrays and objects by name instead of inventing a collection type the
ABI cannot return.

### 7.6 What Phase 6 did not change

The thirty-four verification rows are the same thirty-four rows. The FFI tests,
the SDKs, the batch API, the updater, and the packaging are untouched. The new
surface is Rust-level (`set_module_root`, `allow_fs_read`, `allow_fs_write`) or
script-level (`import`, four builtins); wiring permissions into the CLI and the
SDK constructors is a later increment, so a script run through `omni` can call
`json.parse` but cannot read a file.

---

## 8. Phase 7 — Hardened security, fuzzing, and resource bounds

Phase 7 adds no language feature and moves neither ABI 4 nor language version 2.
It closes the ways a hostile program could make the engine run forever, grow
without bound, or walk out of a granted directory, and it adds a fuzz surface
that is exercised on every verification run.

### 8.1 The bounds, in one table

| Bound | Limit | Where it is enforced | Error |
| --- | --- | --- | --- |
| Instructions per top-level execution | 16,777,216 (`DEFAULT_STEP_LIMIT`, `0` disables) | `Budget::charge` at the top of every VM step | `execution exceeded the step limit of N instructions` |
| VM stack values | 65,536 (512 KiB) | the single `push` helper in the VM loop | `the VM stack reached its 65536-value limit` |
| Parser expression nesting | 128 | `Parser::expr` | `expressions are nested 128 deep, the limit` |
| Call nesting | 64 | parser, unchanged from Phase 3 | `calls are nested 64 deep, the limit…` |
| Module import depth | 16 | module loader, unchanged from Phase 6 | `modules are nested 16 deep…` |
| Arguments per call | 16 | parser | `a call takes at most 16 arguments` |

Every one of these is a positioned runtime or parse error, not a panic, an
abort, or a silently truncated result. The gas error points at the instruction
whose execution was refused; the stack error points at the instruction that
tried to push. Neither depends on wall-clock time, so the same program fails at
the same step on every machine.

### 8.2 Execution gas

`Engine::exec`, `Engine::exec_many`, and the FFI exports above them create one
`Budget` per top-level execution. The budget is passed down through
`run_chunk`, and an `import` hands the **same** budget to the child engine, so a
module cannot reset the count or buy itself a second allowance. A parent whose
own work costs 62 instructions and whose module costs 90 fails at a limit of
100 no matter which side the 100th step lands on, and the failure is attributed
to the module only when it happens inside one.

The default is finite on purpose: an infinite loop is the cheapest denial of
service a script can write, and a language that runs one until the host gives up
has no answer to it. The knob is a Rust policy, not an ABI field:

```rust
let mut engine = Engine::new();
assert_eq!(engine.step_limit(), DEFAULT_STEP_LIMIT);
engine.set_step_limit(10_000);   // tighter allowance for untrusted input
engine.set_step_limit(0);        // explicit opt-out, for a trusted batch
```

`set_step_limit` is Rust-only in Phase 7. The SDKs cannot change it yet, so they
all run under the 2^24 default; adding it to the ABI would be an ABI 5
conversation, and this increment deliberately did not open one.

### 8.3 Failing closed on the filesystem

The Phase 6 write check canonicalized the target and tested containment. That
was correct for an existing file and wrong for a **dangling symlink**: the path
does not exist, so `canonicalize` fails, and an implementation that treated a
failure as "not there yet, use the parent" could create the target through the
link. Phase 7 separates the two cases with `symlink_metadata`:

| Target state | What happens |
| --- | --- |
| Nothing exists at the path | the parent directory is canonicalized and must be inside a granted root; the write is refused otherwise |
| A regular file exists | the file is canonicalized and the result must be inside a granted root |
| A symlink exists (dangling or not) | the link itself is resolved by `canonicalize`; if that fails, the write is refused |
| Any other metadata error | the write is refused with the OS error in the message |

There is no parent fallback for an entry that exists, so a link cannot carry a
write out of the sandbox. Reads already went through canonicalization and are
unchanged. With no roots granted — the CLI's default — both `fs` builtins fail
closed at the first call.

### 8.4 Fuzzing

The fuzz surface is `crates/omni-core/src/fuzz_api.rs`, a `#[doc(hidden)]` module
that exposes the four stages separately:

| Entry point | Input | Invariant |
| --- | --- | --- |
| `lex_only` | arbitrary UTF-8 | returns tokens or a positioned error; never panics |
| `parse_only` | arbitrary UTF-8 | returns an AST or a positioned error; never panics |
| `lower_only` | arbitrary UTF-8 | returns a chunk or a positioned error; never panics |
| `execute` | arbitrary UTF-8 | runs source through the full pipeline under the step limit |
| `execute_bytes` | arbitrary bytes | decodes 8 bytes per instruction and runs the VM directly |

`execute_bytes` is the decoder fuzzer. Each 8-byte word becomes
`(opcode, a, b, c, line, column)`, with the opcode taken modulo the 20 defined
opcodes. Literal spans are read with `str::get`, and `Chunk::name` uses
`str::get` as well, so an operand that points past the source or to the middle of
a multi-byte character yields no text instead of slicing it. Hand-built
instructions can still ask for impossible things — an empty name, a jump to
itself, a call with 65,535 arguments — and each is a positioned error the
deterministic test asserts.

There are two ways to run it, and the script prefers the first:

1. **libFuzzer.** `crates/omni-core/fuzz` is a cargo-fuzz project with four
   targets (`fuzz_lex`, `fuzz_parse`, `fuzz_lower`, `fuzz_vm`) and a seed corpus.
   It is its own workspace because `libfuzzer-sys` needs a nightly toolchain and
   must not become a dependency of the engine's build. On a machine with
   `cargo +nightly` and `cargo-fuzz`:

   ```bash
   bash scripts/fuzz.sh                 # 30 s per target, or set FUZZ_SECONDS
   cargo +nightly fuzz run fuzz_vm crates/omni-core/fuzz/corpus/fuzz_vm
   ```

2. **The deterministic harness**, which is what `scripts/verify-all.sh` runs.
   It needs no nightly and no network: a xorshift64 generator mutates a fixed
   corpus with insert/delete/flip/duplicate edits, every input goes through all
   four pipeline stages and the raw decoder, each run is repeated to prove the
   result is identical, and every error is required to carry `Line >= 1` and
   `Col >= 1`. It ran 1,200 inputs in each of five paths with zero panics on this
   machine. The harness also prints its numbers, so the verification row shows
   how much it actually did rather than just asserting success.

This machine has stable Rust and no network access to fetch `libfuzzer-sys`, so
the cargo-fuzz targets are authored and reviewed but were **not executed here**.
That is stated rather than papered over: the row that ran is the deterministic
harness, and the targets are the same entry points with a different driver.

### 8.5 What Phase 7 did not change

ABI 4, the 16-byte `OmniValue`, the four SDKs, and language version 2 are
untouched. The FFI and SDK tests did not move; the thirty-four rows of Phase 6
still pass byte-for-byte, and one new row runs the fuzz harness, for thirty-five.
The Rust core gained six tests (226 → 233): the step limit, its disable switch
and default, the stack cap, the parser nesting cap, the shared module budget, and
the Unix symlink case. The new public names are `DEFAULT_STEP_LIMIT`,
`MAX_STACK_DEPTH`, `MAX_NESTING`, `Engine::step_limit`, and
`Engine::set_step_limit`; none is an ABI symbol.

---

## 9. Phase 8 — Multi-threading and concurrent execution contexts

The engine had always assumed one handle, one thread. Phase 8 keeps that rule at
the C boundary and removes it inside Rust: an `Engine` is now `Send + Sync`, a
batch can be evaluated across a worker group, and every worker owns its gas
meter, bindings, arena, egress, and module cache.

### 9.1 The thread model, in three layers

| Layer | Rule |
| --- | --- |
| Rust `Engine` | `Send + Sync`. An owned engine may move between threads; `&Engine` may be read from many threads at once. Mutation still needs `&mut`. |
| `Engine::fork` | A full second context: configuration copied, execution history reset. Safe to move to a long-lived worker thread. |
| C `OmniEngine *` | One borrow at a time. Every export still opens a `RefCell`; `omni_exec_parallel` is the supported way to use several cores through one handle, not permission to call two exports concurrently. |

Making the core `Sync` is a claim about interior mutability, not a new lock. Every
method that can allocate, reset, rewind, or write takes `&mut self`. The two types
that appear behind `&self` and could have been a problem — `Arena` and `Slot<T>` —
are asserted in [arena.rs](file:///D:/OmniEngine/crates/omni-core/src/arena.rs):

```rust
unsafe impl Sync for Arena {}
unsafe impl<T: Sync> Sync for Slot<T> {}
```

`Arena` is sound to share by reference because while an `&Arena` is alive no
cursor can move and no chunk can be freed: allocation, reset, rewind, and marking
all require `&mut self`, and a `Value` into an arena slot cannot outlive the arena
it came from. `Slot<T>` is sound when `T: Sync` because `get` is its only
`&self` operation. A compile-time assertion in `engine.rs` makes `Engine` and the
host-call trait fail to build if either bound regresses.

### 9.2 Host functions are now a `Send + Sync` contract

The callback type changed from

```rust
dyn Fn(&Engine, &[Value]) -> HostOutcome
```

to

```rust
dyn Fn(&Engine, &[Value]) -> HostOutcome + Send + Sync
```

and the registry stores an `Arc<HostCall>`, so a fork clones a pointer rather than
a closure. The callback still receives `&Engine`, never `&mut Engine`: it can
observe configuration, not mutate it. Anything the closure captures — including a
C host's `user_data` — must now be usable from several threads, because a worker
may call it. The header states the rule; `HostContext` carries the matching
`unsafe impl Sync` under that documented contract.

Koffi, the Node FFI layer, cannot honour the contract from a worker: it queues a
JavaScript callback to the main thread, and the main thread is blocked inside the
synchronous call. The Node SDK therefore runs a parallel request sequentially
whenever a host function is registered. Python's ctypes callback re-acquires the
GIL from the worker while the call has released it, so it does run there. Rust
callbacks are `Send + Sync` by signature.

### 9.3 A fork copies configuration, not history

`Engine::fork(&self)` returns an engine that looks configured like its parent and
has executed nothing:

| Copied | Reset |
| --- | --- |
| module root, read and write roots | arena, egress block |
| step limit | token, AST, chunk, stack, and scratch buffers |
| globals, with string bytes re-materialized | `let` bindings, module bindings, namespaces |
| host registry (`Arc` clones) | loading stack, diagnostics, counters |
| module cache (new map, shared immutable sources) | `last_value`, `last_error` |

String globals are copied into the worker's own allocation rather than left
pointing at the parent's box, so the parent may redefine a global without changing
what a worker sees. A value the parent can no longer prove it owns is copied as an
inert scalar: a stale read, never a dereference. The fork is a first-class
execution context — an embedder may keep it and reuse it across batches.

### 9.4 Gas and module caches are per engine

Each worker creates its own `Budget` from the inherited step limit, so gas is
deterministic per item: an expensive item is still cut off at exactly the same
instruction count whether it ran alone or beside fifty others. Imports spend from
the item's budget, so importing the same module twice cannot double the work.

The import cache is engine-local, keyed by the canonical path the loader already
computes, and invalidated by file length plus modification time. It is bounded at
64 entries; the insert that would exceed the bound clears the map instead of
growing it. There is no `static` cache anywhere, so two engines — including forked
workers — cannot interfere. `fork()` clones the map, which shares only immutable
`Arc<str>` sources, never the security checks: canonical containment, file type,
size, and UTF-8 validation still run on every import before a cache lookup.

### 9.5 How `exec_parallel` reassembles a batch

`Engine::exec_parallel(&mut self, sources, max_threads)` uses
`std::thread::scope`, so every worker is joined before the call returns and no
global pool or scheduler state outlives it:

1. `max_threads == 0` asks `available_parallelism`; the result is clamped to
   `1..=min(items, 64)`. A value of 1 takes `exec_many` byte for byte.
2. The parent reclaims, then forks one worker per thread.
3. Workers pull source indices from one `AtomicUsize`. `Relaxed` is sufficient:
   the counter orders nothing but itself, and every result returns through
   `join`.
4. Each item runs on its worker's own engine with a fresh `Budget`. A panic is
   caught (`catch_unwind`) and becomes an ordinary `ErrorKind::Panic` item, so one
   bad item cannot take the batch down.
5. Results are reassembled by source index, not completion order. Stats are
   summed; counters are set from the final item.
6. Worker-owned string bytes are copied into the parent's egress only after all
   joins, so all N string results are readable together until the next execution
   — exactly the `exec_many` lifetime.

An empty input returns an empty vector and reclaims nothing.

### 9.6 The ABI and the SDKs

`omni_exec_parallel` is additive in ABI 4:

```c
OMNI_API uint32_t OMNI_CALL omni_exec_parallel(OmniEngine *engine,
                                               const char *const *sources,
                                               const size_t *lengths,
                                               size_t count,
                                               uint32_t max_threads,
                                               OmniValue *out,
                                               uint32_t *statuses);
```

It validates its arrays exactly as `omni_exec_many` does, writes results in source
order, fills `omni_batch_error` for every item, and returns the first failure. A
zero count is a no-op that expires nothing. The header now names the parallel
entry point in the result-lifetime rule (1), the one-handle rule (7), the
re-entrancy rule (9), and the batch rule (11).

Both SDKs expose it and neither breaks against an older library:

- Python: `Engine.run_parallel(sources, max_threads=0)` and
  `try_run_parallel(...)`; `RawEngine.exec_parallel` falls back to
  `omni_exec_many` when the symbol is absent. `max_threads` is validated as a
  non-negative uint32 before marshalling.
- Node: `runParallel(sources, maxThreads=0)` and `tryRunParallel(...)`; the
  declaration is optional and `_batch` uses `execMany` when it is missing or a
  host function is registered.

### 9.7 Verification

The concurrency suite in [tests/concurrency.rs](file:///D:/OmniEngine/crates/omni-core/tests/concurrency.rs)
puts 64 threads on a barrier, gives each a fresh engine under a 5,000-step gas
limit, and requires every answer to be exact, the over-budget program to fail,
and each engine to stay usable with `execs == 3` afterwards. A second test shares
one `Arc<Engine>` read-only from twelve threads and forks it from each. The rest
cover fork semantics, ordered reassembly with failures, single-worker equality
with `exec_many`, concurrent host callbacks through `Arc<AtomicUsize>`, and
module imports through a worker's own engine. Three unit tests in `engine.rs`
cover the module cache: a hit, a length-based invalidation, the bound, and a
fork's independence.

`tests/parallel.rs` in `omni-ffi` is the contract test at the C boundary (null
arrays, empty batch, bad UTF-8, over-long sources, ordering, string lifetimes,
`max_threads` 0 and 1, host callbacks on workers, item accounting). Python has
nine parallel tests including the missing-symbol fallback; Node has ten,
including its own fallback and the host-function rule. Row 36 of
`scripts/verify-all.sh` runs the 64-thread suite and dispatches 64 gas-bounded
items across eight workers from Python, checking every value and the `execs`
delta.

### 9.8 What Phase 8 did not change

ABI 4, the 16-byte `OmniValue`, language version 2, and every existing export are
untouched; `omni_exec_parallel` brings the export table to seventeen. The CLI and
wasm scaffold did not move. The new public names are `Engine::fork`,
`Engine::exec_parallel`, `MAX_PARALLEL_WORKERS`, and `HostCall`'s `Send + Sync`
bounds; the only new ABI symbol is `omni_exec_parallel`.

---

## 10. Phase 9 — Bytecode optimizer, profiler, and diagnostics

Phase 8 made the engine parallel. Phase 9 makes the work it does visible and
smaller: an ahead-of-time optimizer pass runs between lowering and the VM, and
three additive ABI-4 exports hand a host the cost of one execution at opcode
granularity.

### 10.1 One pass, two transformations, one invariant

The optimizer lives in [optimizer.rs](file:///D:/OmniEngine/crates/omni-core/src/optimizer.rs)
and runs once per execution, after lowering and before the VM, on streams the
compiler produced — never on hand-built chunks. Both transformations are keyed
to one rule: **anything removed or rewritten must be provably unable to fail
and unable to have a side effect**, so the sequence of runtime errors, each
with its line and column, is identical with the pass on and off. That is what
"ahead-of-time" buys here: the pass is not a hint, it is a semantics-preserving
rewrite, and the suite in §10.5 asserts the equivalence directly.

**Constant folding** is a linear scan that mirrors the VM's operand stack,
remembering which slots hold compile-time-known numbers. When both operands of
`Add`, `Sub`, `Mul`, `Div`, or `Mod` — or the operand of `Neg` — are constants,
the two pushes and the operator collapse into one `PushFolded`, a new opcode
that carries the `f64`'s 64 bits across its two 32-bit operands and cannot
fail at runtime, because there is no source text left to parse. `Div` and
`Mod` fold only when the divisor is non-zero: "division by zero" is a runtime
error that must keep firing at its own position, so the fold is refused and
the stream is left alone. `Store` keeps its operand's constancy (it pops and
pushes the same value), `Call` poisons the mirror, and every other opcode
mirrors conservatively.

**Dead code elimination** erases a non-final statement whose whole computation
is one constant push followed by the statement-separating `Pop`. The push must
have no remaining way to fail: a number literal that parses, a folded
constant, `nil`/`undef`, or a string whose escapes decoded at compile time. A
string with a bad escape keeps its runtime error. Calls, loads, stores, and
imports are never touched.

**Positions are never rewritten.** A folded constant inherits the operator's
`line`/`column` — the fold for `2 + 3` reports where the `+` was — and every
surviving instruction keeps its own position, so a later error in the same
expression still names the right place.

### 10.2 The pass recycles its buffers like everything else

The `Optimizer` owns two rewrite buffers (`code` and `consts`) held by the
engine and recycled the way the token buffer and the AST are: a warm execution
asks the system allocator for nothing, because the buffers keep the capacity of
the largest program the engine has compiled. Two buffers serve both passes —
the folded stream is written into `code` and swapped in, then the dead-code
pass rewrites *that* into `code` again — and a swap hands the freshly compiled
stream back to the optimizer for next time, so the capacity is never lost.

Three Rust-side knobs, all forked with the engine: `set_optimize(bool)` turns
the pass off for a host that wants to compare streams, `optimize()` reads the
setting, and `last_optimize()` reports `OptimizeStats { folded, elided }` — the
work the pass did to the most recent execution, zeroed when the pass is
disabled or the program failed to compile.

### 10.3 The profiler: one execution, one instruction at a time

[engine.rs](file:///D:/OmniEngine/crates/omni-core/src/engine.rs) gains a
`Profile` the engine fills on every chunk run:

| Field | Semantics |
| --- | --- |
| `total_instructions` | instructions dispatched, successful or not — always the sum of `opcode_calls` |
| `elapsed_ns` | wall-clock nanoseconds of the dispatch loop, measured on every execution |
| `peak_bytes` | bytes the execution arena held when the run ended; allocation is monotonic between reclamations, so this is the execution's peak live heap, not reserved space |
| `opcode_calls` | one count per opcode dispatch — always recorded; the increment is negligible beside the gas charge already paid |
| `opcode_ns` | per-opcode self-time, zero unless profiling is enabled |

The split between the last two rows is the cost decision. Counting costs one
indexed increment per instruction; timing costs two clock reads per dispatch,
bracketing the handler. Counts are therefore always on, and the nanosecond
split is opt-in through `set_profiling(bool)` (also forked, also
engine-local).

The profile resets at the top of every chunk run, so it always describes
exactly the last execution: one single call, one batch item, or the final item
of a batch or a parallel run. A failed execution is profiled like a successful
one — the counters stop where the error did. A fresh engine profiles as all
zeros. Nothing is shared: no static table, so parallel workers and forked
engines cannot observe each other's profiles.

### 10.4 The ABI and the SDKs

Three exports, all additive within ABI 4, bring the table from seventeen to
twenty:

```c
OMNI_API uint32_t OMNI_CALL omni_engine_set_profiling(OmniEngine *engine, uint32_t enabled);
OMNI_API uint32_t OMNI_CALL omni_profile(OmniEngine *engine, OmniProfile *out);
OMNI_API size_t OMNI_CALL omni_opcode_name(uint32_t index, char *buf, size_t cap);
```

`OmniProfile` is a 1,064-byte caller-owned struct — `size`, `opcode_count`,
`total_instructions`, `elapsed_ns`, `peak_bytes`, a `reserved` slot, then two
fixed arrays of 64 `uint64_t`s (`opcode_calls`, `opcode_ns`) — pinned by Rust
const asserts, C `_Static_assert`s, a Python ctypes layout list, and Node
koffi sizeof/offsetof checks, the same four-sided pin `OmniStats` gets.
`omni_profile` reads through the shared borrow (usable any time the handle is
free; `OMNI_ERR_REENTRANT` from inside a callback), always rewrites `out->size`
for the caller to compare, and copies the core's opcode count so a host never
reads past the live part of the arrays. `omni_opcode_name` translates an index
into its mnemonic — `PUSH_FOLDED` included — writing at most `cap - 1` bytes
and NUL-terminating, so a host that cannot read Rust still gets the engine's
own names for its heatmap keys.

Both SDKs follow the additive-export pattern established by
`omni_exec_parallel`: against an older library every existing call keeps
working and only the profile calls fail, with the rebuild instruction in the
message. Python adds `Engine.set_profiling(bool)` and `Engine.profile()` — a
frozen `Profile` with `total_instructions`, `elapsed_ns`, `peak_bytes`,
`opcode_calls`, and `opcode_ns`, the last two as dicts keyed by mnemonic. Node
adds `setProfiling(enabled)` and `profile()`, returning the same facts as
`totalInstructions`, `elapsedNs`, `peakBytes`, `opcodeCalls`, and `opcodeNs`.
Both read the names through `omni_opcode_name`, so the key sets are identical
in both hosts.

### 10.5 Verification

The optimizer suite in
[tests/optimizer.rs](file:///D:/OmniEngine/crates/omni-core/tests/optimizer.rs)
(11 tests) asserts the invariant from both sides: folding respects precedence
and float arithmetic, `Div`/`Mod` by zero survive to runtime at their own
positions, dead statements vanish while calls, stores, and imports never do, a
bad escape is never elided, and `OptimizeStats` resets and honours the switch
and the fork. Three tests answer the brief's performance question with
measurements rather than arguments:

- `optimized_execution_dispatches_fewer_instructions` runs a 10,000-term
  `1 + 1 + …` spine and asserts the optimized stream dispatches 2 instructions
  where the raw stream dispatches 20,002 — a hundred-fold gap, asserted, not
  waved at.
- `dead_strings_make_repeated_executions_faster` puts a dead 40 KiB string
  literal in front of `2 + 3` and sums 500 runs: optimized execution never
  builds the string (`peak_bytes` stays 0) and must beat the raw run's total
  time, which pays to decode and allocate it every run.
- `folding_reduces_gas_consumption` runs `1 + 1 + 1` under a 4-step budget:
  folded, it passes; raw, it dies at the step limit. The optimizer makes gas
  spend smaller, which is the same as making programs faster under a cap.

The profiler suite in
[tests/profiler.rs](file:///D:/OmniEngine/crates/omni-core/tests/profiler.rs)
(6 tests) covers the heatmap summing to the instruction count, the nanosecond
split staying zero until enabled, peak bytes bounding a string-heavy program,
per-execution reset and fork independence, a failed execution profiled like a
successful one, and batch semantics (each item profiled, the last reported).

[tests/profile.rs](file:///D:/OmniEngine/crates/omni-ffi/tests/profile.rs) in
`omni-ffi` is the contract test at the C boundary (7 tests): the struct
crosses with every field intact, every heatmap index names through
`omni_opcode_name`, the toggle controls the nanosecond split, `peak_bytes`
crosses, a borrowed handle refuses profile reads, null arguments are refused,
and the surface is additive within ABI 4. The Python suite adds eight profile
tests and Node adds eight — both including the struct-layout pin and the
missing-symbol fallback.

Row 37 of `scripts/verify-all.sh` (§11) is the cross-host form of the same
facts: the two Rust suites, then `(2 + 3) * 4 - 8 / 2` evaluated in Python and
Node must answer 16.0 with the whole expression collapsed to one `PUSH_FOLDED`
dispatch (four folds, but the stream the VM runs is one push and one `RET`),
zero arithmetic dispatches, the heatmap summing to the instruction count, and
`let x = 1` / `x / 0` still failing at `[Line 2, Col 3]` — folded
optimizations in the same program as an unfoldable division.

### 10.6 What Phase 9 did not change

ABI 4, the 16-byte `OmniValue`, language version 2, and every existing export
are untouched; the three profiler exports bring the table to twenty. With the
pass disabled, the bytecode is exactly what Phase 6 compiled. The CLI and wasm
scaffold did not move. `PushFolded` is the one new opcode, and it exists only
as an optimizer output — the compiler never emits it.

---

## 11. Verification

One command, and it is the acceptance test for the whole brief:

```bash
./scripts/verify-all.sh
```

Measured output on 2026-11-08, with the Phase 9 optimizer and profiler increment in it:

```
== OmniEngine cross-host verification ==
   workspace: /d/OmniEngine

Rust tests... Deterministic fuzz harness... Concurrent execution contexts... Optimizer, profiler, and their heatmaps... Shared library... omni-cli... C embedding... Python SDK... Node SDK... A string is the same bytes in every host... One type error, the same text everywhere... Every surface agrees on the ABI level... String results do not grow memory... A host function answers the same bytes in every host... A host failure is data, and says so identically... Re-entering the engine from a callback is refused, not crashed... The registry says the same thing from both sides... A batch answers the same items in every host... A failing item names itself identically in every host... A batch counts items, not crossings, and reserves nothing extra... A batch inside a host callback is refused, not crashed... The DevPack math, string and type builtins answer the same in every host... A second-line error names line and byte column in every host... eval_str and eval hand back native values... The last execution timer crosses as microseconds... omni --version names what it speaks... a build that matches the release... a newer release is offered... another ABI is refused, not offered... a wrong checksum leaves the target alone... a dry run downloads nothing... a verified update replaces the bytes... a running binary updates itself... 
== Results ==
  PASS  cargo test --workspace (373 tests)
  PASS  fuzz harness: 1200 inputs x 4 stages, 1200 decoder chunks, 146 executed, 0 panics
  PASS  7 concurrency tests, 64 gas-bounded items across 8 workers (6.5ms 9837 items/s)
  PASS  11 optimizer tests, 6 profiler tests, one folded program profiled identically in: python node
  PASS  omni.dll (6.6M)
  PASS  omni run examples/hello.omni -> 42
  PASS  omni run examples/strings.omni -> hello, world 😀
  PASS  examples/c/embed.c -> 42 via omni.h
  PASS  python ctypes -> 42.0
  PASS  python SDK tests (140 tests)
  PASS  node koffi -> 42
  PASS  node SDK tests (ℹ tests 134 ℹ pass 134 ℹ fail 0 )
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
  PASS  math, string and type builtins reduce identically in: cli python node
  PASS  line 2, column 13, and the operator's message in: cli python node
  PASS  int/number, str/string, bool/boolean with no wrapper in: python node
  PASS  one 200 KB execution timed, non-zero and in microseconds: python(930us) node(1423us)
  PASS  omni --version -> omni 0.1.0 (language v2, ABI 4)
  PASS  omni check-update -> 0, omni 0.1.0 is up to date (ABI 4)
  PASS  omni check-update -> 10, omni 99.0.0 is available; this build is 0.1.0
  PASS  omni check-update -> 11, release 99.0.0 speaks ABI 5; this build speaks ABI 4
  PASS  omniup update -> 3, target unchanged
  PASS  omniup update --dry-run -> 10, nothing fetched
  PASS  omniup update -> 0, target now the release bytes
  PASS  the replaced binary still runs: omni 0.1.0 (language v2, ABI 4)
  PASS  omni update -> 0, the running image is kept at omni.exe.bak

All checks that could run, passed.
```

`verify-all.sh` tests the four hosts against a checkout build. The release
artifacts are checked separately, because they are what a consumer gets and
nothing in the suite above touches them: a wheel installed into a clean venv
outside the repository, an extracted npm tarball, and a C archive compiled
against its own header. Those runs are recorded in section 6.7, with the sizes
they produced.

The second row is the deterministic fuzz harness (§8.4): 1,200 mutated inputs
through the lexer, parser, lowering pass, and VM, plus 1,200 decoder chunks
built from arbitrary bytes, with two runs of every input required to agree and
every error required to carry a real line and column. It is a row rather than a
one-off because it is cheap (well under a second) and because a regression in a
decoder or a position is exactly the kind that a happy-path suite cannot see.

The third row is the concurrency increment (§9): the Rust suite's seven tests
put 64 threads on a barrier with independent, gas-bounded engines and share one
`Arc<Engine>` read-only from twelve; the Python half dispatches 64 programs
across eight workers and requires every answer in source order plus an `execs`
delta of exactly 64. A lost write, a shared cursor, or an item claimed twice
would all break that accounting.

The fourth row is the Phase 9 increment (§10): the optimizer and profiler
Rust suites, then the same folded program in Python and Node. Each host must
answer 16.0 for `(2 + 3) * 4 - 8 / 2` with the whole expression collapsed to
one `PUSH_FOLDED` dispatch and zero arithmetic dispatches, its heatmap must sum
to its instruction count, and `let x = 1` / `x / 0` must still fail at
`[Line 2, Col 3]` with the division's own message. The row is the cross-host
form of the invariant the optimizer suite asserts in Rust: the fold changed
the cost, not the answer or the positions.

The cross-host and update rows are the ones that could not exist in one host. The string
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
same refusal. The four DevPack rows cover the toolkit surface: a program built
from the new math, string and type builtins reduces to the same value in the CLI,
Python, and Node; a second-line error renders `[Line 2, Col 13]` and the same
message in all three; `eval_str` and `eval` hand each host a native value rather
than a wrapper; and a 200 KB program's measured execution time crosses as a
non-zero microsecond count in both SDKs.

The eight update rows are different in kind: they run the real binaries against
a manifest written to a temporary directory. `check-update` is asked three times
— a matching version must exit 0, a newer one 10, a different ABI 11 — and the
installer is attacked four ways. A wrong checksum must leave the target's bytes
untouched; a dry run must fetch nothing and still exit 10; a good manifest must
replace the file with the exact payload, which is then executed to prove it
runs; and a self-update must succeed while keeping the running image beside it
as `omni.exe.bak` on Windows, where a running executable can be renamed but not
deleted. On Unix the same row asserts no backup is left.

The script degrades to `SKIP` rather than lying. `cargo`, `gcc`, `python` and
`node` each have their own guard, Node without `koffi` is skipped with
`npm install koffi` in the reason, and each cross-host row skips itself when fewer
than two hosts answered. Nothing was skipped on this machine — all thirty-seven
rows ran. The Rust row runs the suite once into a log and reads its exit status;
an earlier version piped cargo into `grep` under `pipefail`, which let a failing
suite inherit a successful-looking pipeline and briefly hid a stale CLI test. `omni-wasm` has no row here because `wasm32-unknown-unknown` is not
installed; its 7 tests run under `cargo test --workspace`, where the crate
compiles for the host target and asserts its status codes against the same
constants the C header declares.

Against the brief's five phases:

| Phase | Asked for | Verified by |
| --- | --- | --- |
| 1 | Workspace on D:, fast compiles, managed `target/` | `cargo test --workspace` 373 tests; `target/` measured at 351 MB |
| 2 | Ultra-fast memory-safe core, no GC | NaN-boxed 8-byte `Value` asserted for size; arena reset is O(chunks); egress rewind keeps reservations flat over 100,000 executions; no collector exists to pause |
| 3 | C ABI callable from Python and JavaScript | same `42` and the same `hello, world 😀` from Rust, C, Python, Node; 20 exports confirmed with `objdump -p`; `omni.h` compiles warning-free under `-Wall -Wextra` |
| 4 | A custom extension GitHub recognises | `.gitattributes` tier 1; the tier-2 limit is stated in section 5.3 rather than glossed over |
| 5 | `pip`/`npm` SDKs that do not move the language stats | 140 + 134 SDK tests; `linguist-vendored` on `sdks/**`; release packages carry a CI-built engine, checkouts carry none |

Test totals: **261** in `omni-core` (237 unit, 7 in `tests/concurrency.rs`, 11 in
`tests/optimizer.rs`, 6 in `tests/profiler.rs`), **78** in `omni-ffi` (21 unit,
13 in `tests/batch.rs`, 20 in `tests/ffi_contract.rs`, 8 in
`tests/host_callbacks.rs`, 9 in `tests/parallel.rs`, and 7 in
`tests/profile.rs`), **27** in `omni-cli` (12 library tests for the updater and
the writer, 10 for `omni`, 5 for `omniup`), **7** in `omni-wasm`, **140**
Python, **134** Node. The batch, callback, parallel, and profile suites are
contract tests against the header, not unit tests of the implementation they
sit next to.

Static checks are part of the definition of done:

```bash
cargo clippy --workspace --all-targets -- -D warnings   # clean
cargo fmt --all --check                                  # clean
```

---

## 12. Storage ledger

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

## 13. Troubleshooting

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

**`execution exceeded the step limit of 16777216 instructions`** — the program
ran longer than the default allowance, usually because it imported a module in a
loop that never terminates. It is the gas check doing its job, not a bug: the
same source fails at the same instruction on every machine. A trusted embedder
can raise it with `engine.set_step_limit(n)` or disable it with `0`; the SDKs
cannot, because the limit is a Rust policy rather than an ABI field. A deep
syntax error is separate: expressions past 128 levels fail in the parser with
`expressions are nested 128 deep, the limit`, before any gas is spent.

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

## 14. What comes next

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
fits in a minute. Nothing in that round changed the language or the ABI. A fifth
round added the developer toolkit without moving the ABI either: seventeen
math, string and type builtins, errors that point at a line and byte column in every host, an
execution timer in four bytes that were already padding, and native-value sugar
(`eval_str` / `eval`) in both SDKs. A sixth replaced the one-pass evaluator with
an AST and a bytecode VM — still behind unchanged ABI 4 — and added `import`,
the module sandbox, filesystem permissions, and scalar JSON. A seventh hardened
the existing surface without adding a feature: execution gas shared across
modules, a VM stack cap, a parser nesting cap, a symlink-safe write check, and a
fuzz surface with both libFuzzer targets and a deterministic harness. An eighth
made the core shareable across threads without moving the C contract: `Engine`
is `Send + Sync` and forkable, gas and module caches became engine-local, a batch
can run over a worker group (`omni_exec_parallel`), and both SDKs expose the
parallel shape with a sequential fallback. A ninth made the bytecode cheaper to
run and its cost visible: an ahead-of-time optimizer that folds constants and
erases dead statements without moving an error position, and opcode-granularity
profiles through three additive exports. The next
language increment is still the one section 3.7 lists — control flow,
collections, and user-defined functions.

Release-side, three things are deliberately unfinished rather than unknown:

- **0.1.0 is packaged, not published.** The workflow builds every artifact and
  the local runs in section 6.7 passed, but PyPI and npm have not been touched.
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
2. **String operations.** `len`, `slice`, `contains`, and `replace` shipped,
   but indexing, comparison order, and a way to get a number or a bool *into* a
   string are still missing. The box already stores the length, so this is
   evaluator work, not layout work.
3. **A zero-copy result path for a host that wants one.** The harnesses in 4.9
   say the crossing is 36–240 ns and the wrapper is microseconds, and the
   wrapper's biggest single cost is copying string bytes out. A borrow API —
   pointer valid until the next execution, on the same rule 1 a host already
   lives under — is the largest performance increment still on the list, and
   the benches are the instrument it will be judged by. A batch already makes
   all N results live at once, so a borrow and a batch compose.
4. **User-defined functions and control flow.** The AST and the VM are in place:
   `Jump`, `JumpIfFalse`, and `Call` are defined, and the branch opcodes are
   tested against hand-built chunks. What remains is a function value, a frame,
   the syntax that emits branches, and a decision about whether the arena still
   resets per execution once a frame can outlive a statement.
5. **wasm for real.** `wasm-bindgen`, the `wasm32-unknown-unknown` target, and a
   browser example; the shape is already scaffolded and asserted to agree with
   the C statuses. A wasm string will need its own copy rule, since there is no
   shared address space to rewind inside, and a host callback needs a `JsFunction`
   rather than a function pointer.
6. **Linguist tier 2.** The upstream PR from section 5.3, which would also make
   the new `examples/strings.omni` show up under its own language name.
7. **A step-limit knob for the SDKs, and continuous libFuzzer.** `set_step_limit`
   is Rust-only in Phase 7; exposing it (and a per-call override) is an ABI 5
   decision, deliberately not taken here. The cargo-fuzz project is ready, but
   this machine has no nightly toolchain, so running it on a schedule is a CI
   change rather than a code change.
8. **Parallel host callbacks from Node.** `runParallel` falls back to the
   sequential path whenever a host function is registered, because koffi
   queues a JavaScript callback onto the main thread and `omni_exec_parallel`
   blocks that thread while it waits. Python does not have the problem: ctypes
   releases the GIL around the call. Lifting the Node restriction needs an
   asynchronous entry point that hands the batch to libuv and settles a promise
   later, which is a new API shape rather than a port of the existing one.

**One cosmetic defect, deliberately left for its own commit.** `display_kind`
already quotes a symbol — `'='` — and two parse-error templates wrap it in quotes
again, so `let = 1` reports `found ''=''`. It is in `crates/omni-core/src/ast.rs`
at the `format!` calls in `expect_ident` and its parser helper, and no test
asserts the doubled form, so it is a safe first issue. It stayed out of this
increment because the strings work changed the ABI and the memory model, and a
commit that does both is a commit whose regressions cannot be attributed. The SDKs
quote the doubled text in their READMEs so that what is documented is what runs
until the fix lands.

Deferred on purpose, not forgotten: no GC (there is nothing to collect), no
incremental compilation tricks beyond `incremental = true`, no dependency
graph to cache.
