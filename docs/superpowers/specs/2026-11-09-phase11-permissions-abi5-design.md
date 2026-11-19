# Phase 11 — Enterprise ABI v5 & Cross-Language Permission Control

Status: implemented. ABI 4 → 5. Language v2 unchanged. Version stays 0.1.0
(unpublished); the release tag remains the version decision.

## 1. Goals and non-goals

Goals:

1. Three fail-closed, thread-safe FFI exports — `omni_engine_allow_fs_read`,
   `omni_engine_allow_fs_write`, `omni_engine_set_module_root` — that
   configure the sandbox dynamically per engine instance, at runtime, through
   the same borrow-guarded handle every other entry point uses.
2. SDK surfaces on both wrappers: pythonic `engine.allow_fs_read(path)`,
   `engine.allow_fs_write(path)`, `engine.set_module_root(path)` and the
   node-idiomatic `engine.allowFsRead(path)`, `engine.allowFsWrite(path)`,
   `engine.setModuleRoot(path)`. A non-permitted operation keeps raising the
   engine's existing deterministic, position-aware runtime error; nothing in
   the host process is touched by a denied program.
3. An ABI bump to **5**, declared identically by `omni.h`, the Rust crate,
   the Python SDK, and the Node SDK, so every host that pins its ABI
   constant refuses a mismatched engine rather than reading new offsets.
4. Row 39 of `scripts/verify-all.sh`: a Rust FFI permission suite plus
   Python and Node programs that each build their own sandbox directory,
   prove denial, grant, prove access, and report ABI 5 — self-contained per
   host, so the row never passes a path across host boundaries.

Non-goals: permission *revocation* (clear_* APIs), per-program permission
scopes, read-only vs write-only path rules beyond the two root sets,
plugin-function permissions, CLI flags, and any language change.

## 2. Why the ABI bumps now

Phases 8–10 added exports without a bump, and the additive pattern served:
a host built against an older header simply lacks the new symbol and the
SDKs fall back. Phase 11 declares the permission surface to be part of the
contract *by name*: an enterprise host that configures a sandbox wants the
engine to refuse to run if the configuration surface is absent, not to
silently run with an empty sandbox. The bump is the honest way to say
"this surface is required from v5 on." Concretely:

- `OMNI_ABI_VERSION` moves 4 → 5 in `omni-ffi`, `omni.h`, the plugin
  fixture's manifest copy, `sdks/python/omni/_ffi.py`, and
  `sdks/nodejs/lib/bindings.js`. All existing exports keep their exact
  signatures; nothing else in the ABI moves.
- `omni_check_update` compares against 5 from the same code that compared
  against 4.
- The plugin loader's `omni_abi` check refuses any plugin manifest built
  against ABI 4. That is fail-closed and intended: the fixture bumps with
  the header.
- `scripts/check-versions.py` already reads the ABI from `omni.h`, `_ffi.py`
  and `bindings.js` and demands one value across all surfaces, so the bump
  is enforced mechanically.

## 3. The core surface already exists — and is already isolated

`Engine::allow_fs_read`, `Engine::allow_fs_write` and
`Engine::set_module_root` shipped in Phase 6. Phase 11 adds nothing to
their semantics, because they are already the right semantics:

- **Canonicalize once at grant time.** A relative and an absolute path to
  the same directory produce the same permission; every containment check
  compares a canonical path against a canonical root.
- **Fail closed.** Empty root sets mean every `fs` call is refused with a
  positioned error naming the rule (`fs.read_text is disabled: …`,
  `path '…' is outside the allowed read roots`, `import '…' escapes the
  module root`). Writes resolve symlinks before the containment check.
- **Deduplicated.** Granting the same root twice is one root.
- **Fork snapshots, not shares.** `Engine::fork` deep-copies
  `module_root`, `fs_read_roots`, and `fs_write_roots`. A grant on the
  parent after the fork is invisible to the child, and a grant on one
  worker is invisible to its siblings. `omni_exec_parallel` builds its
  workers by fork, so parallel execution inherits the isolation for free.

Phase 11's core work is therefore tests, not machinery: a fork-snapshot
test (grant, fork, grant more on the parent, the child still denies the
new root) and a parallel test (workers inherit the grant the parent held
before the batch; a sibling engine without the grant stays denied).

## 4. The three FFI exports

Each follows the `omni_engine_load_plugin` path discipline exactly:

```c
uint32_t omni_engine_allow_fs_read(OmniEngine*, const char* path, size_t path_len);
uint32_t omni_engine_allow_fs_write(OmniEngine*, const char* path, size_t path_len);
uint32_t omni_engine_set_module_root(OmniEngine*, const char* path, size_t path_len);
```

Return contract, in order:

- `OMNI_ERR_ARG` for a null engine or path, a zero length, a length over
  `OMNI_MAX_PATH` (4096, the same ceiling plugin paths get — a permission
  path longer than that is a configuration bug, not a path), bytes that
  are not UTF-8, or a path that cannot be canonicalized (missing parent,
  permission error on the host). Canonicalization is the fail-closed
  step: the engine refuses to record a root it could not resolve.
- `OMNI_ERR_REENTRANT` when called from inside a host callback on the same
  handle — the existing `borrow_mut` guard answers, with no new mechanism.
- `OMNI_OK` on a recorded grant. Re-granting an existing root is `OMNI_OK`
  and a no-op; `set_module_root` replaces the previous root, matching the
  Rust API.

The path is length-prefixed, so it may contain any UTF-8 the host
platform allows in a path; no NUL terminator is required or consulted.

`omni.h` gains `#define OMNI_MAX_PATH 4096u`, the three prototypes with
the same contract text, and `OMNI_ABI_VERSION 5u`. The header's comment
explaining why the ABI had stayed at 4 gains one sentence: v5 is the
permission surface becoming part of the contract.

## 5. SDK surfaces

**Python** (`omni` package, ctypes). `_ffi.py` declares the three
functions with the additive `hasattr` guard (a library built before v5
still serves every other call). `RawEngine` gains `allow_fs_read`,
`allow_fs_write`, `set_module_root`: each encodes the path as UTF-8
bytes, calls through, and raises `OmniError` on any non-OK status; a
missing symbol raises `OmniLibraryError` naming the export and the
rebuild command, exactly like `set_profiling`. The public `Engine`
exposes the three pythonic names, accepting `str` or `os.PathLike`.

**Node** (`omniengine` package, koffi). `bindings.js` declares the three
with the try/catch-null pattern (koffi throws at declaration time).
`Engine.allowFsRead(path)` / `allowFsWrite(path)` / `setModuleRoot(path)`
throw `OmniLibraryError` when the symbol is absent and `OmniError` on a
non-OK status. The wrapper rejects a path containing `'\0'` with a
`TypeError` before the call: koffi would truncate at the NUL while the
length counts the full string, and a path with an embedded NUL is not a
path on any platform this engine runs on.

Neither SDK unwraps engine errors into exceptions for *executed
programs*: a denied `fs` call inside a program is a runtime error the
program sees, reported by `last_error` / `run` exactly as before. The
host-level methods only fail when the *host* misuses them.

## 6. Verification

- `crates/omni-ffi/tests/permissions.rs` (new suite): ARG arms (null
  engine, null path, zero length, over-length, non-UTF-8, missing
  directory); re-entrancy (a registered callback calls
  `omni_engine_allow_fs_read` on its own engine → `OMNI_ERR_REENTRANT`);
  denial-then-grant through the FFI for read and write, asserting the
  positioned error text on denial and the value on grant; module root via
  FFI (temp dir, module file, import succeeds, escape refused); engine
  isolation (two engines, one granted — the other stays denied); fork
  snapshot isolation; `omni_exec_parallel` inherits the parent's grant
  and a fresh sibling stays denied.
- `sdks/python/tests/test_permissions.py`: the public `Engine` API —
  denial raises a positioned `OmniError` whose text names the rule,
  grant flips read/write, `set_module_root` makes `import` work,
  re-grant is a no-op, a bad path raises `OmniError` with the argument
  status, and ABI is 5.
- `sdks/nodejs/tests/permissions.test.js`: the same matrix through
  `allowFsRead` / `allowFsWrite` / `setModuleRoot`, including the NUL
  guard, with `node:test`.
- **Row 39** of `scripts/verify-all.sh`: builds the fixture-free FFI and
  runs the Rust suite, then runs one inline Python program and one inline
  Node program. Each program creates its own temp directory, proves a
  read is denied (prints the error text), grants read, proves the value,
  proves write denial, grants write, proves the file, sets a module root
  and imports a module it wrote — and prints its host's ABI. The row
  greps for the three hosts' outputs and fails on any divergence. No
  path crosses a host boundary anywhere in the row.

## 7. Deliberately deferred

- **Revocation** (`clear_fs_read` and friends) — the Rust core already
  has `clear_module_root`; a symmetric clear for the two root sets is a
  later, equally additive surface.
- **Per-program permission overrides** — permissions are engine policy,
  not program policy; nothing in v5 lets a program ask for more.
- **Plugin permissions** — a plugin runs at host-callback privilege and
  is trusted code by construction; sandboxing plugin *code* is out of
  scope.
- **CLI flags** — `omni run --allow-read` would sit on top of this
  surface; the CLI keeps failing closed until someone asks for it.
