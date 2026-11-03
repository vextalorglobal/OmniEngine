# OmniEngine Phase 1 — Design Specification

Status: approved (2026-11-02)
Scope: repository skeleton plus one runnable vertical slice. The real parser, IR,
stdlib, and Smi tagging are explicitly out of scope and deferred to Phase 2.

## Problem statement

OmniEngine is a custom programming language with a polyglot ecosystem: a Rust core,
a C ABI consumed by Python and JavaScript, and a GitHub-visible language identity.
Phase 1 has to prove that a `.omni` source file can cross the FFI boundary and
return a correct value from three independent host runtimes, without baking in a
memory model that Phase 2 cannot extend.

## Environment (verified, not assumed)

| Fact | Value | Consequence |
|---|---|---|
| `D:\OmniEngine` | existed, empty, no git | clean slate, `git init` safe |
| D: free | 76 GB of 138 GB | the stated 20 GB budget is not binding on D: |
| C: free | **7.5 GB, 93 % used** | the real constraint; `CARGO_HOME` growth lands here |
| rustc / cargo | 1.99.0 / 1.99.0 at review, **1.99.1** at implementation | current stable features available |
| default toolchain | `stable-x86_64-pc-windows-msvc` | plus `windows-gnu` installed — **outcome: the workspace is pinned to `windows-gnu`, see the linker note below** |
| `link.exe` on PATH | false positive — Git's hard-link utility | verified empirically: it shadows MSVC's linker, so the workspace pins GNU |
| Visual Studio | `D:\VS` | rustc self-locates VS; fallback to GNU is available |
| gcc | MinGW-W64 16.2.0 on PATH | `windows-gnu` cdylib path works without VS |
| `wasm32-unknown-unknown` | not installed | wasm crate must be excluded from default build |
| wasm-pack / wasm-bindgen-cli | not installed | wasm build deferred, scaffolded only |
| sccache | not installed | skipped: installing costs ~300 crate compilations on C: |
| Python / Node / npm | 3.15.0 / 26.12.0 / 11.20.0 | Node 26 ABI rules out `ffi-napi` |

Decision recorded from review: `CARGO_HOME` stays on C:. The guide documents the
relocation as an opt-in and does not perform it.

## Architecture

### Repository layout

```
D:\OmniEngine\
├── Cargo.toml              workspace, resolver = "3", default-members
├── rust-toolchain.toml     pin stable windows-gnu (amended: MSVC was tried first)
├── .cargo/config.toml      build policy, checked in
├── .gitattributes          two-tier linguist strategy
├── .gitignore              target/, CARGO_HOME escape hatch
├── README.md
├── docs/
│   ├── GUIDE.md            initialization and execution guide
│   └── superpowers/specs/  this document
├── crates/
│   ├── omni-core/          src/{lib,value,arena,lexer,eval}.rs
│   ├── omni-ffi/           src/lib.rs, include/omni.h, cbindgen.toml
│   ├── omni-cli/           src/main.rs
│   └── omni-wasm/          scaffold only, non-default member
├── sdks/
│   ├── python/             pyproject.toml, omni/{__init__,_ffi}.py
│   └── nodejs/             package.json, index.js, lib/bindings.js
├── examples/hello.omni
└── tests/                  test_ffi.py, test_ffi.js
```

### Module boundaries

`omni-core` knows nothing about C, Python, or Node. It exposes `Value`, `Arena`,
`Engine`, and `exec`. `omni-ffi` depends on `omni-core` and is the only crate
permitted to name a `#[repr(C)]` type or an `extern "C"` function. `omni-cli` and
`omni-wasm` are both thin consumers of `omni-core`, which keeps the core honest:
if it can only be driven through the FFI, the boundary is wrong.

Dependency direction is strictly `omni-ffi → omni-core` and `omni-cli → omni-core`.
`omni-core` has no workspace-internal dependencies and no required external crates.

## Value representation

`Value` is `#[repr(transparent)] struct Value(u64)` using NaN boxing. The tag space
lives in the IEEE-754 NaN regions, which ordinary floating-point arithmetic can
never produce.

```
tag test              representation
--------------        ----------------------------------------
is double       →     every box that matches neither rule below
+Inf / -Inf     →     0x7FF0_0000_0000_0000 / 0xFFF0_0000_0000_0000  (doubles)
double NaN      →     0x7FF8_0000_0000_0000  (canonical qNaN, slot 0 reserved)
pointer         →     0x7FF7_XXXX_XXXX_XXXX  (48-bit address in the sNaN region)
null/true/false →     0x7FF8_0000_0000_000{1,2,3}
undefined       →     0x7FF8_0000_0000_0004
```

The classification order matters. `is_pointer` tests `v >> 48 == 0x7FF7`, and
`is_singleton` tests `v >> 32 == 0x7FF8_0000` with a nonzero low-32 field below 16.
Everything else is a double.

An earlier draft tagged pointers as `0x0000_XXXX_XXXX_XXXX`, on the reasoning that
x86-64 user addresses have zero high bits. That is wrong: `0.0` is itself
`0x0000_0000_0000_0000`, so `is_pointer(0.0)` would return true and every zero in a
program would be read as a null dereference. The same trap catches every denormal
below roughly 2^-969. Pointers therefore take the signaling-NaN region, which IEEE
arithmetic never generates, and still yield a full 48-bit address.

Incoming `f64::NAN` is normalised to the canonical qNaN on construction so that
`is_nan()` on a boxed double is unambiguous, and so an arbitrary quiet NaN
arriving from a host cannot be mistaken for a tag slot.

Recorded tradeoff: the singletons occupy only the 16 low bits of the canonical-NaN
row, so there is room for a handful more tags but not for a small-integer tag
alongside a 48-bit pointer. Phase 1 stores every number as `f64`. Integers are
exact to 2^53 and scalar arithmetic is one SSE instruction with no allocation, so
the Smi win would be allocation that is already not happening. Introducing Smi in
Phase 2 is a breaking change to `Value`, which is why the pointer region is fixed
here rather than left implicit.

## Memory model

`Arena` is a bump allocator over fixed-size boxed chunks. Compound values receive
a `#[repr(C)] Object { tag, payload }` header allocated inside the arena. The
arena is dropped in one deallocation when the owning `Engine` context ends.

There is no garbage collector and no reference counting, so there is no collection
pause and no per-assignment cost. The price is that object graphs cannot outlive
their arena and cycles are not a concern because nothing is tracked. Phase 1 does
not need either capability.

Safety invariant, enforced structurally: **no arena pointer crosses the C
boundary.** `Engine::exec` returns tagged results by value; strings are copied
into caller-owned storage with an explicit length. A `*const Object` handed to
Python would outlive the arena and become a use-after-free.

## FFI surface

Crate types are `cdylib` and `staticlib`. The exported surface is deliberately
small:

```
omni_version()                       → const char *
omni_engine_new()                    → OmniEngine *        (nullable)
omni_engine_free(eng)                → ()
omni_exec(eng, src, len, out*)       → OmniStatus
omni_exec_number(eng, src, len, out*)→ OmniStatus
omni_last_error(eng, buf, cap)       → usize
```

`OmniStatus` is `OMNI_OK`, `OMNI_ERR_COMPILE`, `OMNI_ERR_RUNTIME`,
`OMNI_ERR_PANIC`, `OMNI_ERR_ARG`. `OmniValue` is `#[repr(C)]` with a tag and a
payload, so a host never has to reimplement NaN boxing to read a result.

**Amended during implementation: the payload is a `double`, not a union.** The
design said `union { double number; const char *string; }`. Phase 1 has no
string values, so the pointer arm would be a variant nothing populates that every
host still has to read unsafely, and a union makes the read a correctness hazard
in six languages instead of a documented tag check in one. The shipped type is

```c
typedef struct OmniValue { uint32_t tag; double number; } OmniValue;
```

size 16, `number` at offset 8 — pinned by a Rust `const _: () = assert!(..)` pair,
`_Static_assert` in `omni.h`, `ctypes.sizeof` at Python import, and `koffi.sizeof`
at Node load. It becomes a union when strings arrive, which is the point where a
pointer arm earns its keep.

Two further ABI rules, both found by testing rather than by design:

- `omni_last_error` copies the **rendered** error (`line 1: expected a name,
  found '='`), not `error.message`. Dropping the location across the boundary
  leaves a host unable to point a user at their source.
- `omni_exec(eng, NULL, 0, out)` is a legal empty program: it returns `OMNI_OK`
  with `out.tag == OMNI_UNDEF`, while `omni_exec(eng, NULL, n, out)` for `n > 0`
  is `OMNI_ERR_ARG`. Node hands an empty `Buffer` over as a null pointer, and
  `slice::from_raw_parts(null, 0)` is undefined behaviour even though it reads
  nothing.

Every entry point body runs inside `std::panic::catch_unwind`. A Rust panic
unwinding into Python or native code is undefined behaviour and is the most common
defect in hand-rolled Rust FFI, so panics become `OMNI_ERR_PANIC` plus a captured
message. Every entry point also null-checks its engine pointer and validates its
buffer capacity before writing.

`omni.h` is hand-written for Phase 1. `cbindgen.toml` is committed so the
anti-drift generator is a one-command upgrade rather than a redesign.

On x86-64 Windows there is a single calling convention, so `extern "C"` matches
`__cdecl` and ctypes needs no convention annotation.

## Host SDKs

Python uses stdlib `ctypes` with zero pip dependencies. Library discovery order is
`OMNI_ENGINE_HOME`, then `target/release/omni.dll` relative to the repo, then
platform names `omni.dll` / `libomni.so` / `libomni.dylib` on the system path.
Absence of the library raises a message naming the build command, not a bare
`OSError`.

Node uses `koffi`, deliberately instead of the conventional `ffi-napi`:
`ffi-napi` is unmaintained and does not compile against Node 26's ABI, so it
requires node-gyp and a compiler chain to install. `koffi` ships prebuilt
binaries. If `koffi` is missing the SDK raises an actionable install error rather
than a native crash, and the wasm build is documented as the offline fallback.

Both SDKs expose the same two-call shape — `exec(source)` and `eval_number(source)`
— so a host binding is not a second language design surface.

## GitHub language identity

The premise that `.gitattributes` can create a language named "OmniEngine" is
false: `linguist-language=` remaps a file to a language Linguist already knows and
cannot mint a new one. Strategy is therefore two-tier.

Tier 1, in the repository now. `*.omni linguist-language=Rust` gives counting and
highlighting today, choosing Rust as the closest syntax for `let` bindings and
`//` comments. `/sdks/** linguist-vendored=true` and `/docs/**
linguist-documentation=true` remove those lines from language statistics while
keeping them browsable, which is the actual mechanism behind the requirement that
SDK code not pollute repo statistics. `*.omni linguist-detectable=true` keeps the
extension visible.

Tier 2, a documented checklist for a future `github/linguist` pull request: a
`languages.yml` entry with extensions and colour, plus a Chroma or Prism lexer for
highlighting. GitHub closes most new-language submissions until the language has
adoption outside its own repository, so tier 2 is gated on real usage rather than
scheduled as a Phase 1 deliverable.

Extension chosen: `.omni` over `.ox`. `.ox` is already claimed by Ox and Oxide and
reads ambiguously. `.omni` collides with the OmniGraffle document format, but
repository-local overrides make that collision cosmetic rather than functional.

## Build and storage policy

One workspace, single shared `target/`. `default-members` excludes `omni-wasm` so
`cargo build` succeeds without the wasm32 target installed.

`debug = "line-tables-only"` for dev is the highest-value knob available: full
line-info debug on MSVC emits gigabytes of PDB per crate and is a measurable
compile-time cost, which matters directly against the 7.5 GB free on C:. Dev also
gets `incremental = true` and `codegen-units = 256`; release gets `lto = "thin"`
and `codegen-units = 1` for the performance claim.

`sccache` is not installed and not required for Phase 1.

Linker strategy: attempt the default `windows-msvc` cdylib build first and record
the real result; fall back to `--target x86_64-pc-windows-gnu` using the installed
MinGW GCC if the MSVC link fails. Neither path is asserted without testing.

**Outcome, recorded 2026-11-03: the MSVC attempt failed and the workspace is
pinned to `windows-gnu`.** `cargo build --target x86_64-pc-windows-msvc` reported
`link: missing operand after '■'`, which is not a Rust error: `where link`
resolves to `C:\Users\m7004\.qoder\bin\git\usr\bin\link.exe`, Git for Windows'
coreutils hard-link utility, which shadows MSVC's
`D:\VS\VC\Tools\MSVC\14.51.36231\bin\Hostx64\x64\link.exe` and takes unrelated
arguments. Fixing it properly means reordering `PATH` machine-wide or always
building from a Developer Command Prompt; pinning the workspace to GNU changes
this project only, and its `ld` is unambiguous. The machine was not authorised
for `PATH` surgery, so `rust-toolchain.toml` pins
`stable-x86_64-pc-windows-gnu` with the diagnosis in its comments.

The MSVC route remains documented in the guide and is **not tested**, because
installing an MSVC Rust toolchain to check would cost several hundred megabytes on
the drive the build policy exists to protect. Nothing in the workspace depends on
the ABI: no build script, no C compilation, and the SDKs look for `omni.dll`,
which both toolchains emit. What differs is the import-library naming —
`libomni.dll.a` / `libomni.a` on GNU versus `omni.lib` / `omni.dll.lib` on MSVC.

## Acceptance criteria

1. `cargo test` passes for value round-trip, NaN normalisation, arena determinism,
   and the evaluator.
2. `cargo run -p omni-cli -- examples/hello.omni` prints `42`.
3. `python tests/test_ffi.py` prints `42` through ctypes.
4. `node tests/test_ffi.js` prints `42` through koffi, or reports the precise
   reason it could not run.
5. `git check-attr` confirms `.omni` maps to Rust and `/sdks` is vendored.
6. A deliberate engine panic returns `OMNI_ERR_PANIC` rather than crashing the host.

Criterion 4 is the only one that depends on network access for `npm install koffi`
and will be reported honestly if it cannot be met.

## Out of scope for Phase 1

Full grammar, AST and resolver; bytecode compiler and VM; stdlib and strings
beyond the FFI copy path; Smi tagging; garbage-collected cycles; the actual wasm
build; the Linguist pull request; publishing to PyPI or npm.

## Implementation outcome (2026-11-03)

Every acceptance criterion was met, measured rather than asserted.

| # | Criterion | Result |
|---|---|---|
| 1 | `cargo test` for value round-trip, NaN normalisation, arena determinism, evaluator | met — 74 tests across four crates |
| 2 | CLI prints `42` | met — `omni run examples/hello.omni` → `42` |
| 3 | ctypes prints `42` | met — 29 Python tests plus the smoke check |
| 4 | koffi prints `42` | **met** — `npm install koffi` succeeded (26 Node tests), and koffi 3.3.6 loads on Node 26 from its bundled prebuild with no install script |
| 5 | `git check-attr` maps `.omni` to Rust and `/sdks` to vendored | met |
| 6 | a deliberate panic returns `OMNI_ERR_PANIC` | met — asserted from Rust, C, Python, and Node, and the same engine still evaluates `1 + 1` afterwards |

Deviations from this document, each recorded where it is load-bearing:

1. **`OmniValue` is a struct, not a union** (amended above). It becomes a union
   with strings.
2. **The toolchain is pinned to `windows-gnu`, not MSVC** (amended above).
3. **The library target is named `omni`**, not `omni_ffi`, so the artifact a host
   loads is `omni.dll` — which is what `omni.h` and both SDKs promised from the
   start. The manifest comment and the built file disagreed until this was fixed.
4. **`debug = "line-tables-only"` was kept**, and it did what the design said it
   would: `target/` measures 175 MB (114 MB `deps`, 58 MB `incremental`). Because
   the workspace has no dependencies, nothing was fetched into `CARGO_HOME` — the
   only file under it touched during the whole build is cargo's own
   `.global-cache` metadata, and the 982 MB total is unchanged.
5. **Two SDK API names changed under review.** Python's `eval` became `evaluate`
   — shadowing the builtin in a review surface invites `exec`-adjacent confusion —
   and Node's `OmniError` now matches Python's `.message` semantics, after a
   constructor bug where assigning `this.message` discarded the status prefix.
6. **Library discovery had to be reworked twice.** An installed wheel sits in
   `site-packages`, nowhere near `target/debug`, so both SDKs walk upward from the
   package *and* from the working directory, and each walk checks its starting
   directory before ascending. The measured matrix is in the guide, section 6.3.

Nothing in the "out of scope" list above moved into Phase 1.
