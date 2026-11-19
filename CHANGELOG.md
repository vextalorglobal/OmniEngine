# Changelog

All notable changes to OmniEngine, newest first. Until the first published
release the version stays `0.1.0`; the compatibility currency is the ABI
level, which is **5** since the permission increment — v2, v3, and v4 grew
the surface additively, and v5 moved it on purpose for the first time. Entries
are grouped by the eleven phases of the brief (one spec per phase in
`docs/superpowers/specs/`), not by patch version.

## [Unreleased]

### Added

- **Enterprise permission exports (Phase 11).** ABI v5 adds three fail-closed
  exports — `omni_engine_allow_fs_read`, `omni_engine_allow_fs_write`,
  `omni_engine_set_module_root` — that configure the sandbox per engine
  instance at runtime, each `(engine, path, path_len) -> status` with a 4 KB
  path cap (`OMNI_MAX_PATH`). Python gets `Engine.allow_fs_read(path)` and
  friends for any `str` / `os.PathLike`; Node gets `allowFsRead` and friends
  with byte-buffer marshalling. Both wrappers refuse empty, over-long, or
  NUL-bearing paths before crossing, and raise `OmniLibraryError` — with the
  rebuild command in the message — when the loaded engine predates the
  exports.

### Security

- **The permission boundary is now part of the contract.** Denied reads,
  writes, and imports fail closed before the disk is touched, with the same
  positioned messages Phase 6 defined. Grants are engine-local and
  canonicalization-proof: forks inherit them, so every `omni_exec_parallel`
  worker shares the parent's roots, while sibling engines never see a grant
  made to another handle. Granting from inside a host callback is
  `OMNI_ERR_REENTRANT`, never a second mutable borrow; non-permitted
  operations raise deterministic `OmniError`s without crashing the host.

### Verification

- **39 of 39** cross-host rows pass in `./scripts/verify-all.sh`: 406 Rust
  tests (264 core, 93 FFI, 27 CLI, 15 JIT, 7 wasm), 140 Python, 142 Node.
  Row 39 runs the eight FFI permission contract tests — malformed grants,
  uncanonicalizable paths, deny→grant round trips, untouched disks on denial,
  module-root gating, re-entrancy refusal, and sibling isolation — then
  repeats the deny→grant→write→import→isolation sequence through the Python
  and Node wrappers.
- `python scripts/check-versions.py` reports one version (0.1.0) and one ABI
  (5) across `omni.h`, both SDKs, and the CLI updater; `cargo fmt --all
  --check` and `cargo clippy --workspace --all-targets -- -D warnings` stay
  clean.
- **Distribution verifier and artifact gate (Phase 12).**
  `scripts/package-verify.py` unpacks every archive, wheel, and npm tarball
  and proves, from file bytes alone: the container magic matches the claimed
  platform (`MZ` / `\x7fELF` / Mach-O, thin and fat), the three ABI 5
  permission export names are present in each engine binary — a binary-level
  ABI proof that needs no `objdump` — the VGPIL license text ships in the
  artifact, the bundled header and all four repository ABI declarations agree,
  and the `.sha256` sidecar matches. The npm tarball is checked per platform
  bundle; `--require-all-platforms` demands all five for a release. The
  release workflow now gates publishing on it: `package-verify` runs after the
  wheels and tarball exist, and `pypi`, `npm-publish` (split out of
  `npm-package` so the tarball is proven before it is published), and
  `github-release` all wait on it. Verify-all row 40 is the local form:
  package the release library, build a wheel and an npm tarball from it, run
  the verifier over all of them, then load each from its unpacked form —
  the wheel imported via `PYTHONPATH` with no pip install, the tarball
  extracted outside the checkout — and both must answer 42.

## [0.1.0] — 2026-11-08

**Status:** built and verified locally on Windows x64. Not yet published to
PyPI or npm — publishing is a tag push and the two credentials in
`docs/RELEASING.md`. Nothing below is a forecast; every number is measured and
reproducible with `./scripts/verify-all.sh` (`docs/GUIDE.md`).

### Added

- **Engine core (Phase 2).** A zero-dependency Rust engine with no garbage
  collector. Scalars are NaN-boxed into 8 bytes; `OmniValue` is the 16-byte
  ABI struct at level 4. All allocation flows through a bump arena that is
  bulk-freed per evaluation, and the egress block that holds results handed to
  a host rewinds instead of freeing — 100,000 executions reserve exactly what
  the first one did, and a warm engine allocates nothing.
- **C ABI and header (Phase 3).** A hand-written `omni.h` with static layout
  assertions, an export table verified against it, and an embedding example
  that compiles warning-free under `-Wall -Wextra`. ABI mismatches are refused
  at load, not read as garbage.
- **Strings and egress memory (Phase 2 increment).** Strings cross the
  boundary as length-prefixed egress blocks; `omni_str_copy` refuses addresses
  it cannot prove, and `omni_define_string` gives hosts a safe way to hand
  bytes back. ABI v2, folded into the stable ABI 4.
- **Host callbacks (Phase 3).** A script calls a host function by name with an
  arity checked before foreign code runs; `omni_register_host_fn` /
  `omni_unregister_host_fn` / `omni_host_fn_count` manage the table. A host
  exception surfaces as a readable error naming the function, and a callback
  that re-enters its own engine gets `OMNI_ERR_REENTRANT` instead of a second
  mutable borrow.
- **Batch execution (Phase 4).** `omni_exec_many` / `run_many` / `runMany`
  carries up to 4,096 programs in one crossing with checkpointed arena
  reclamation; a failing item reports its own status and text without stopping
  the rest. ABI v4.
- **Developer toolkit (DevPack increment).** Twenty-one builtins needing no
  host registration: math (`abs`, `min`, `max`, `clamp`, `floor`, `ceil`,
  `round`, `pow`, `sqrt`), text (`len`, `trim`, `upper`, `lower`, `contains`,
  `replace`, `slice`), inspection (`type_of`), and sandboxed I/O and data
  (`fs.read_text`, `fs.write_text`, `json.parse`, `json.stringify`). Errors
  name a position as `[Line 2, Col 13]` with a byte column identical in every
  host; `last_exec_micros` carries the last execution time through bytes that
  were already padding; `eval_str` / `eval` hand back native values.
- **Bytecode VM and modules (Phase 6).** Source is lexed, parsed into a flat
  AST, lowered into bytecode, and run on a stack VM — all behind the unchanged
  ABI 4. Language v2 adds `import "path"` with namespace isolation: imported
  code sees builtins but not the embedder's globals or callbacks, and escapes,
  cycles, and nesting past 16 are refused.
- **Parallel execution (Phase 8).** `omni_exec_parallel` / `run_parallel` /
  `runParallel` spreads one batch over up to 64 forked engines, each with its
  own arena, egress block, gas meter, bindings, and module cache; results come
  back in source order and all string results stay readable together. The Rust
  `Engine` is `Send + Sync` and forkable.
- **Bytecode optimizer (Phase 9).** An ahead-of-time pass folds constant
  arithmetic into one `PushFolded` dispatch and erases dead constant
  statements, provably unable to fail or to move an error position: a
  10,000-term addition collapses from 20,002 dispatches to 2.
- **Opcode-granularity profiling (Phase 9).** `omni_engine_set_profiling`,
  `omni_profile`, and `omni_opcode_name` hand back instructions dispatched,
  wall-clock time, peak arena bytes, and per-opcode heatmaps through
  `profile()` in both SDKs — three additive ABI-4 exports.
- **JIT machine-code tier (Phase 10).** An optional Cranelift tier compiles
  the expression subset of a chunk into one native function with a
  deopt-on-error seam: any condition the VM would answer with an error
  re-runs the same chunk on the VM, so error text exists in exactly one place
  and positional accuracy is 100% by construction. Measured 56x over the VM on
  a 2,000-term sum (9,484 ns/run vs 532,286 ns/run).
- **Native plugin loader (Phase 10).** `omni_engine_load_plugin` loads a
  `.dll` / `.so` exporting `omni_plugin_manifest`, verifies the manifest
  fail-closed against a bounded window, registers every function through the
  existing host-callback ABI, and pins the library for the engine's lifetime.
  Plugins run under the same containment contract host callbacks already have,
  with panic containment documented and tested.
- **Self-updating CLI (updater increment).** `omni update` verifies a
  release manifest's SHA-256 and replaces the binary by rename, restoring the
  old file on failure; `omniup` updates an `omni` that is not the running
  process. Both SDKs expose the same comparison (`check_updates()` /
  `checkUpdates()`) through the engine's own `omni_check_update` export.
  Plain HTTP is refused by default.

### Security

- **Execution gas (Phase 7).** Every top-level execution carries a
  deterministic budget of 16,777,216 VM instructions shared with imported
  modules; runaway loops and recursion fail as positioned errors, never
  panics.
- **Bounded stacks (Phase 7).** VM stack capped at 65,536 values, parser
  nesting refused past 128, module nesting past 16, and a 128-level guard on
  the lowering pass so expression depth costs engine-owned heap, not the
  host's stack.
- **Symlink-safe writes (Phase 7).** Filesystem writes resolve symlinks
  before the containment check, so a link cannot carry a write outside a
  granted root.
- **Fail-closed plugin verification (Phase 10).** Every manifest read is
  bounds-checked against a bidirectional window and copied out before the
  image is trusted; a hostile library can get itself refused but cannot make
  the verifier read past the window.
- **Fuzzing on every verification run (Phase 7).** Four cargo-fuzz targets
  (lexer, parser, lowering, decoder) plus a deterministic in-repo harness that
  mutates 1,200 inputs through all five paths per `verify-all.sh` run, with
  output determinism and positioned errors asserted.

### Performance

- Three zero-dependency benchmark harnesses (`scripts/bench.sh`) measuring
  one crossing per host; all performance claims in the docs are quoted runs.
- Constant folding and dead-code elimination ahead of the VM (Phase 9).
- Cranelift machine-code tier at 56x over the VM on straight-line sums, with
  the VM as the untouched fallback (Phase 10).

### Packaging

- Python wheel with the engine inside (`omni/_native/`), zero install-time
  dependencies; npm tarball carrying five engines under
  `native/<platform>-<arch>/`; C archives with header, import library,
  example, and license.
- Tag-driven CI builds all of it on five native runners — Windows x64, Linux
  x64/arm64, macOS x64/arm64 — into PyPI, npm, and a GitHub Release
  (`.github/workflows/release.yml`).
- A 60-second quickstart (`docs/RELEASING.md` §1) and a release runbook that
  includes the not-yet-done steps honestly (PyPI/npm publishing).

### Verification

- **38 of 38** cross-host rows pass in `./scripts/verify-all.sh`: 395 Rust
  tests (261 core, 85 FFI, 27 CLI, 15 JIT, 7 wasm), 140 Python, 134 Node,
  the C embedding example, the deterministic fuzz harness, the parallel and
  JIT throughput proofs, and the updater attack rows.
- `cargo fmt --all --check` and `cargo clippy --workspace --all-targets --
  -D warnings` are clean; `python scripts/check-versions.py` reports one
  version (0.1.0) and one ABI (4) across every surface.

### Changed

- **License (post-`9aab27b`).** MIT gives way to the Vextalor Global Public &
  Integration License (VGPIL) 1.0 on every packaged surface: free
  integration, production, and commercial use; no standalone resale or
  re-licensing of the raw source or core binary. Code published under MIT
  before the switch keeps those grants.

## Prior releases

None — 0.1.0 is the first release, and it has not been published yet.
