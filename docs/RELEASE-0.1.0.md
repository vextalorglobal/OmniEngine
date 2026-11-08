# OmniEngine v0.1.0 — November 2026

The first release with a stable ABI, both SDKs, and packaged artifacts. Every
number below was measured on the development machine and is reproducible with
the commands in [GUIDE.md](GUIDE.md); nothing here is a forecast.

## Highlights

**Core engine.** Rust, no garbage collector. Scalars are NaN-boxed into 8 bytes;
`OmniValue` is the 16-byte ABI struct at level 4. Every allocation goes into a
bump arena that is bulk-freed at the start of each evaluation, and the egress
block that holds results handed to a host rewinds instead of freeing. The
measured consequence: 100,000 executions reserve exactly what the first one did,
and a warm engine records 0.00 allocations per execution.

**Interoperability.** One program returns the same bytes through Rust, C,
Python, and Node. Python binds through `ctypes` with an empty dependency list;
Node binds through `koffi`; C uses the hand-written `omni.h`. Both SDKs refuse an
engine whose ABI differs from their own constant rather than reading the wrong
offsets.

**Callbacks and batches.** A script calls a host function by name, with an arity
the engine checks before foreign code is entered. A Python exception or a
JavaScript `throw` becomes a readable host error at the call site, and a callback
that re-enters its own engine gets `OMNI_ERR_REENTRANT` rather than a second
mutable reference. `omni_exec_many` / `run_many` / `runMany` carries up to 4,096
programs in one crossing; a failing item reports its own status and text without
stopping the rest.

**Parallel execution.** `omni_exec_parallel` / `run_parallel` / `runParallel`
spreads one batch over up to 64 forked engines. Each worker owns its arena,
egress block, gas meter, `let` bindings, and module cache; the batch still
returns in source order, the first failure is the call's status, and all string
results stay readable together. The Rust `Engine` is `Send + Sync` and can be
moved or read across threads, while the C handle keeps its one-borrow-at-a-time
rule. The parallel entry point is additive in ABI 4, and both SDKs fall back to
the sequential batch against a library that predates it. In Node, a registered
host function forces that sequential path, because koffi can only enter
JavaScript on the main thread.

**A bytecode VM behind the same ABI.** Source is lexed, parsed into a flat AST,
lowered into bytecode, and run on a stack VM. The lowering pass never recurses,
so expression depth costs heap the engine already owns rather than the host's
stack. `omni_exec` and `omni_exec_many` did not move: ABI 4 is unchanged and
`OmniValue` is still 16 bytes. Language version 2 adds `import "path"`, which
loads a module into an isolated namespace — a fresh child engine that sees
builtins but not the embedder's globals or callbacks, exports top-level names
that do not start with `_`, and refuses escapes, cycles, and nesting past 16.
The module root and filesystem roots are opt-in Rust APIs, so imports and reads
fail closed by default.

**Hardened execution.** A program cannot run forever or grow without bound. Each
top-level execution carries a deterministic budget of 16,777,216 VM
instructions, shared with every module it imports, and a backward jump or an
infinite recursion fails with `execution exceeded the step limit of N
instructions` at the offending instruction. The VM stack is capped at 65,536
values, the parser refuses expressions nested past 128, and filesystem writes
resolve symlinks before the containment check, so a link cannot carry a write
outside a granted root. The limits are positioned errors, never panics, and they
apply identically through Rust, C, Python, and Node because they live below the
unchanged ABI. Fuzzing ships in two forms: four cargo-fuzz targets for the
lexer, parser, lowering pass, and instruction decoder, and a deterministic
harness that mutates a fixed corpus through all five paths on every
`verify-all.sh` run — 1,200 inputs per path, with deterministic output and
positioned errors asserted.

**A standard toolkit.** Twenty-one builtins need no host registration. Math:
`abs`, `min`, `max`, `clamp`, `floor`, `ceil`, `round`, `pow`, `sqrt`. Text:
`len`, `trim`, `upper`, `lower`, `contains`, `replace`, `slice`. Inspection:
`type_of`. Sandboxed I/O and data: `fs.read_text`, `fs.write_text`,
`json.parse`, `json.stringify`. `slice` takes character offsets so it cannot cut
a UTF-8 character in half, while `len` still counts bytes; `sqrt` of a negative
and a `pow` that overflows fail loudly instead of returning `NaN` or infinity.
`json` handles the four scalar kinds a 16-byte value can carry and refuses
arrays and objects by name; `fs` accepts only paths inside a root the embedder
granted. A runtime error names its position as `[Line 2, Col 13]` with a byte
column that reads identically in every host, a wrong argument count says
`[Line 1, Col 1]: Mismatched arity in call 'clamp': takes 3 arguments, got 2`,
`last_exec_micros` carries the most recent execution time in microseconds
through bytes that were already padding, and `eval_str` / `eval` hand back
native platform values instead of a wrapper. Neither SDK unwraps into a list or
a dictionary, because ABI 4 has no collection type for a program to produce.

**Staying current.** `omni update` reads a release manifest over HTTPS, verifies
the SHA-256 it names, and replaces the binary by rename, restoring the old file
if any step fails. `omniup` updates an `omni` that is not the running process.
Both SDKs can report the same answer (`omni.check_updates()`,
`checkUpdates()`) without downloading anything, and the comparison itself lives
in the engine's `omni_check_update` export so every surface agrees on what
"newer" means. Plain HTTP is refused by default.

**Quality.** 36 of 36 cross-host verification rows pass on this machine: 348 Rust
tests, 131 Python, 126 Node, plus the C embedding example and the deterministic
fuzz harness. `cargo fmt --check` and `cargo clippy -- -D warnings` are clean.

**Packaging.** A platform wheel carries the engine in `omni/_native/`; the npm
tarball carries five engines under `native/<platform>-<arch>/`; each C target
ships an archive with header, import library, example, and license. A tag builds
all of it on five native runners — Windows x64, Linux x64/arm64, macOS
x64/arm64 — into PyPI, npm, and a GitHub Release.

## Status of this release

- The artifacts are built and verified locally. **0.1.0 is not yet published**
  to PyPI or npm; publishing is a tag and the two credentials in
  [RELEASING.md](RELEASING.md).
- Linux wheels are tagged `manylinux_2_39` and require glibc 2.39 or newer.
  `auditwheel repair` is not run; the tag states the build host's floor.
- Windows wheels and archives are x64 only.
- The Windows C archive is MSVC-built; the C example in this tree was verified
  against the GNU archive.

## Ownership and license

- Owner: Tamjid Abdur Rahman
- Engineering organization: Vextalor Global
- License: Vextalor Global Public & Integration License (VGPIL) 1.0

VGPIL grants free integration, production, and commercial *use*, including
inside Python, Node, and C packages. It does not permit selling or re-licensing
the raw source or core binary as a standalone or rebranded product, and
attribution and the license file must remain intact. The full text is
[LICENSE](../LICENSE).

Code published before this license was applied — through commit `9aab27b` — was
released under MIT, and those grants are not withdrawn retroactively.
