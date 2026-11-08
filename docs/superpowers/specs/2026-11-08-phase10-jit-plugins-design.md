# Phase 10 — JIT Machine Code Compilation & Native Plugin Architecture

Status: implemented. ABI 4 unchanged (one additive export). Language v2 unchanged.
Deviations from this design, discovered during implementation, are recorded
inline in §2, §4, §5, §8 and §10.

## 1. Goals and non-goals

Goals:

1. An optional JIT compilation tier, in a new workspace crate `omni-jit`, that
   compiles a bytecode chunk's expression subset to native machine code with
   Cranelift and executes it. Every path the JIT cannot or will not run falls
   back to the bytecode VM, so the sequence of runtime errors, each with its
   line and column, is bit-identical to the VM's.
2. A native plugin loader in `omni-ffi` that loads `.dll` / `.so` libraries
   exporting an `omni_plugin_manifest` symbol, verifies the manifest strictly,
   and registers the plugin's functions as host functions through the existing
   `OmniHostFn` ABI — with panic containment and the library pinned for the
   engine's lifetime.
3. Rust test suites proving JIT/VM bit-equality, a JIT throughput win over the
   VM on the same chunk, and manifest verification refusing malformed plugins.
   Row 38 of `scripts/verify-all.sh` runs both suites plus a cross-host
   differential in Python and Node.

Non-goals: JIT for strings, loads, stores, calls, or imports; tiering
thresholds (the language has no loops, so "hot" is meaningless yet);
cross-execution JIT caches; per-opcode profiling of JIT'd runs; SDK bindings
for plugin loading; any ABI or language bump. `omni_exec_parallel` and every
existing export keep their exact signatures.

## 2. Why a JIT is possible at all today

Lowering emits no `Jump` or `JumpIfFalse` yet (the grammar has no `if` and no
`nil`), so every chunk the compiler produces is straight-line: a sequence of
pushes and operations ending in `Ret`. A straight-line chunk is a leaf
function, and a leaf function with no references to static data needs no
relocations. That is what makes the tier a few hundred lines: generate the
function, compile it, copy it into executable memory, call it.

The subset the JIT compiles — the *JIT-safe* opcodes — is exactly the
expression language:

`PushNumber`, `PushFolded`, `PushNil`, `PushUndef`, `Add`, `Sub`, `Mul`,
`Div`, `Eq`, `Ne`, `Neg`, `Pop`, `Ret`.

`PushStr`, `Load`, `Store`, `Call`, `Import`, `Jump`, `JumpIfFalse` are
refused: they need the arena, the binding table, host calls, or control flow,
and every one of them is a reason to stay on the VM.

*Deviation:* `Mod` moved from the safe list to the refused list — Cranelift
0.121 has no `frem` instruction, and emulating C `fmod` bit-exactly (including
its sign rules) is not worth a software loop when the VM already has it. A
chunk containing `Mod` stays on the VM, which also keeps "remainder by zero"
a VM-positioned error by construction.

## 3. Deopt-on-error: positional accuracy by construction

The JIT executes only the happy path. It never formats an error message. Any
condition that would make the VM raise — a non-number operand, a division or
remainder by zero — branches to a single *deopt* exit that returns status 1.
The caller then runs the **same chunk** on the VM, which raises the canonical
positioned error at its own line and column.

Consequences:

- Error text exists in exactly one place (the VM), so JIT and VM cannot drift.
  The contract is: **for any program, the sequence of runtime errors, each
  with its line and column, is identical whether or not the JIT tier ran.**
- A chunk the safety check refuses never reaches native code; its VM run is
  indistinguishable from a deopt's.
- The JIT's results are not "close": packing must be bit-exact (§4), so the
  differential test compares `Value::bits()` equality, not tolerance.

## 4. Semantics the JIT must reproduce bit-for-bit

The VM's rules, taken from `omni-core`'s dispatch:

- **Boxing.** A computed `f64` is pushed through `Value::from_f64` semantics:
  a NaN result collapses to the canonical quiet NaN (`0x7FF8_0000_0000_0000`),
  anything else keeps its raw bits. `PushFolded` applies the same rule to its
  carried bits. `PushNil` / `PushUndef` push the singleton tag bits
  (`Value::NIL.bits()` / `Value::UNDEF.bits()`), and `Eq`/`Ne` push
  `Value::TRUE.bits()` / `Value::FALSE.bits()`.
- **The number check.** An operand is a number exactly when it is a double:
  not the pointer row (`bits >> 48 != 0x7FF7`) and not a singleton
  (`bits >> 32 == 0x7FF8_0000` with `0 < low32 < 16`). Anything else
  deopts, and the VM raises its own "type mismatch" error. The canonical NaN
  *is* a number — low32 is zero — and must keep propagating, not deopt.
- **`Div`.** `b == 0.0` (which includes `-0.0`) deopts; the VM says
  "division by zero".
- **`Eq` / `Ne`.** Numbers compare by IEEE (`NaN != NaN`, `0.0 == -0.0`),
  which Cranelift's `fcmp` gives directly. Non-number operands deopt and the
  VM answers (its tag rule is bits equality — identical to what a JIT'd
  comparison would say, but the deopt keeps the JIT number-only).

*Deviation:* `Mod` never reaches native code (see §2), so there is no
remainder-by-zero deopt to define; the VM raises it with full position.

`PushNumber` literals are parsed at compile time from the source span; the
lexer guarantees they parse, and a chunk whose literal does not is simply
refused like an unsafe opcode.

## 5. Code generation

One Cranelift function per chunk:

```
fn jit_chunk() -> u64
```

- *Deviation from the `(stack, out) -> u32` sketch:* the generated function is
  **parameterless with a single `I64` return**. The compiled body writes its
  temporaries into its own machine stack frame; the only thing that crosses
  the call is the result. The value comes back as its bits, and a deopt is the
  same channel carrying a sentinel bit pattern the tier can never produce as a
  value: `DEOPT_BITS = 0x7FF8_0000_0000_00FF` — the canonical-NaN row with a
  low-word payload outside both the singleton slots and the canonical quiet
  NaN (which `Value::from_f64` collapses to slot 0). A single `i64` return
  lands in `rax` on SysV and Win64 x86-64 alike and in `x0` on aarch64, so the
  entry point transmutes to `unsafe extern "C" fn() -> u64` with no ABI
  variance across hosts. (A two-register `u128` return was measured and
  rejected: this host's toolchain returns `u128` in `xmm0` while Cranelift
  hands back register pairs — the sentinel-on-one-channel design replaced it.)
- Stack slots are compile-time state: each opcode reads/writes fixed offsets
  in the compiled frame, and a compile-time mirror of Cranelift SSA values
  tracks what each slot holds. The chunk's **statically computed maximum
  depth** bounds the frame, so no overflow is possible. `Pop` pops the mirror;
  `Ret` returns its value (or `UNDEF` for a `Ret`-less hand-built chunk,
  matching the VM).
- Arithmetic emits `fadd` / `fsub` / `fmul` / `fdiv` / `fneg` followed by the
  NaN-collapsing select; `Div` branches to the deopt block on a zero divisor.
- Compile once per `Jit` with `cranelift_codegen` + `cranelift_frontend`;
  the finalized `MachBuffer` is copied into executable memory.

**Executable memory is hand-rolled, not `cranelift-module`/`cranelift-jit`.**
A leaf function needs no relocations, so the two crates — and their
`object`/`region` dependency tree — buy nothing here. The module is ~40
cfg'd lines: `VirtualAlloc(PAGE_EXECUTE_READWRITE)` + `FlushInstructionCache`
on Windows, `mmap(PROT_READ|PROT_WRITE|PROT_EXEC)` + `__clear_cache` on
Linux and macOS. `Jit::drop` releases the allocation.

**Architecture.** `cranelift-native` selects the host ISA, so the same code
generates for x86_64 and aarch64. This machine is x86_64, so aarch64
execution is untested; a compile-only test builds the function for an
explicit aarch64 triple and asserts a non-empty machine-code buffer, which
proves the codegen path is architecture-clean without executing it.

## 6. The pipeline surface in omni-core

The JIT crate must see the stages `Engine::exec` hides, and it must be able
to hand the VM a chunk it compiled. `omni-core` gains a small public surface
— the same one the fuzz binaries use, promoted and documented:

- `bytecode::{OpCode, Instr, Chunk, compile, disassemble}` become `pub`
  (fields included); `Ast::parse_into` becomes `pub`.
- `optimizer::optimize_chunk(&mut Chunk, &str) -> OptimizeStats` — a public
  wrapper over the existing pass, with its own scratch buffers.
- `Engine::run_compiled(&mut self, chunk: &Chunk, source: &str)` — the
  existing `run_chunk_for_fuzz`, promoted under an honest name; it is the
  deopt path.

`Engine` itself gains **no** JIT flag and **no** dependency: the zero-dep
storage decision stands, and the tier lives entirely in `omni-jit`. A host
opts in by calling `omni_jit::exec_with(&mut engine, source, optimize)`,
whose contract is: *same `Result<Value, OmniError>` as `Engine::exec`,
bit-for-bit, errors included.* Two documented deltas: a JIT'd success does
not touch the engine's `execs`/`last_value`/`last_error` accounting, and
JIT'd instructions do not appear in the profile heatmap or spend step-limit
gas — both moot today, because the safe subset has no calls, imports, or
jumps, so a JIT'd chunk terminates by construction and the VM fallback still
charges normally.

## 7. The plugin ABI

One exported symbol, `omni_plugin_manifest`, pointing at:

```c
typedef struct OmniPluginFn {
  const char*      name;         /* UTF-8, NUL-terminated */
  uint32_t         arity;        /* <= OMNI_MAX_ARGS (16)  */
  uint32_t         reserved;     /* zero */
  OmniHostFn       call;         /* the function itself   */
} OmniPluginFn;

typedef struct OmniPluginManifest {
  uint32_t         magic;         /* 0x4F4D4E49 = "OMNI"  */
  uint32_t         size;          /* sizeof(manifest)      */
  uint32_t         abi_version;   /* 1                     */
  uint32_t         omni_abi;      /* must equal omni_abi_version() */
  const char*      name;          /* UTF-8, NUL-terminated */
  uint32_t         version_major, version_minor, version_patch;
  uint32_t         function_count;  /* <= 64               */
  const OmniPluginFn* functions;
  uint64_t         reserved;      /* zero; 8-alignment     */
} OmniPluginManifest;
```

The `call` field is the existing `OmniHostFn` shape, so a plugin function is
a host function: `args` is a borrowed view of `argc` `OmniValue`s valid until
return, the value comes back in `out`, and `OMNI_HOST_OK` / `OMNI_HOST_ERR`
is the only failure channel. No new calling convention, no new value
representation.

## 8. Loading, verifying, registering

`omni-ffi/src/plugin.rs`, using a hand-rolled loader (no `libloading`
dependency — `omni-ffi` stays one-dependency): `LoadLibraryW` +
`GetProcAddress` on Windows, `dlopen(RTLD_NOW)` + `dlsym` on Unix, cfg'd.

1. **Load** the library (`RTLD_NOW` on Unix, so missing imports fail at load,
   not at first call).
2. **Resolve** `omni_plugin_manifest`; absence is an error naming the symbol.
3. **Verify, fail-closed, in order**: magic; `size == sizeof` (this also
   catches a fixture that compiled against a drifted manifest); abi_version;
   `omni_abi == omni_abi_version()`; name non-null, UTF-8, non-empty, within
   `MAX_IDENTIFIER_LEN`; `function_count <= 64`; `functions` non-null when
   the count is positive; per function: name valid as above, `arity <=
   MAX_ARGS`, `call` non-null. Any failure reports the specific check and
   unloads the library.
   *Deviation — the window is bidirectional.* Every read is bounds-checked
   against a window: for the real-library path, 64 KiB **below and above**
   the manifest symbol (this toolchain's linker placed the function table
   224 bytes *below* the symbol, so an upward-only window refused a valid
   plugin); for the test door `verify_bytes`, exactly the supplied slice.
   A hostile library can get itself refused, but cannot make the verifier
   read past the window or keep a pointer alive past the load.
4. **Copy, then forget the library's memory.** The manifest's names are
   copied into owned Rust structures; after verification nothing is ever read
   out of the plugin image again — only its function pointers are kept.
5. **Register** each function on the target `Engine` via
   `register_host_fn`, wrapped so a panic inside the plugin becomes
   `HostOutcome::Failure("plugin function panicked: …")` — an error that
   names the function and the payload, never an unwind across the VM.
   Registration follows the engine's existing rule: re-registering a name
   replaces the previous function, exactly like `omni_register_host_fn`.
   *Deviation — panics are the plugin's own to catch.* On Windows, a panic
   that unwinds out of a statically-linked-std cdylib is a **foreign
   exception**: the host's `catch_unwind` cannot catch it (the process
   aborts), because the exception belongs to the plugin's runtime. The
   contract therefore documents itself as: plugins must catch their own
   panics at their exported boundaries and report the message as a host
   failure (the fixture's `crash` is the pattern); the engine-side
   `catch_unwind` trampoline remains the last line of defense for plugins
   that share its runtime.

**Pinning.** A registered function pointer is only sound while the library
stays loaded. The Rust `plugin::Plugin` type owns its library handle and
documents the same contract host callbacks already have ("keep the plugin
alive as long as the engine, or unregister first"). The C export below goes
further and makes the rule structural.

**Memory isolation, honestly.** Plugins run at the same isolation boundary
every host callback already has: values cross the FFI boundary as copies,
engine memory addresses are never handed out, and a plugin that writes
outside its own memory can still corrupt the process — a `.dll` in the
process is the process. What the loader adds is fail-closed verification
before any plugin code runs, panic containment when it does, and a defined
lifetime for the code itself.

## 9. One additive C export

`uint32_t omni_engine_load_plugin(OmniEngine*, const char* path, size_t path_len)`
— loads and verifies a plugin and registers every function on the engine,
storing the loaded library in a registry owned by the handle. The library is
therefore pinned for the engine's lifetime and freed at `omni_engine_free`,
never while a registration points into it. Status codes follow the house
style (`OMNI_ERR_ARG` for a bad path or a failed verification; the Rust API
returns a `PluginError` with the specific check for hosts that want the
detail). ABI stays **4**: this is an addition, like Phase 9's three.

## 10. The plugin fixture

A workspace member `crates/omni-plugin-fixture` (cdylib, zero dependencies)
exporting a valid manifest and three flat-named functions: `double(x) -> 2x`,
`add(a, b)`, and `crash()` which panics — the containment test. (*Deviation:*
the §10 example wrote `pluginfixture.double`; the implemented names are flat,
matching the host-call registry today — §12's namespacing stays deferred.)

It deliberately does **not** depend on `omni-ffi` (that would close a
dev-dependency cycle: `omni-ffi`'s tests depend on the fixture as an
artifact). The fixture declares its own copy of the two `#[repr(C)]`
structs; that is the C ABI's normal state of affairs, and drift fails
closed: the loader's `size` and `omni_abi` checks reject any copy that
does not match this crate's layout. Because the fixture statically links
std, it follows the §8 panic contract: `crash` catches its own panic at the
export boundary and reports the payload as a host failure.

*Deviation — build order.* Cargo **artifact dependencies** proved
unavailable: on the pinned cargo 1.99.1 they still require `-Z bindeps`
(unstable). The fallback was adopted instead: `omni-plugin-fixture` is a
**default workspace member** (unlike `omni-wasm`), so a plain
`cargo build` produces the DLL, and the tests locate it at
`target/debug/omni_plugin_fixture.dll` (searching `target/release` second)
at run time, skipping with a hint when it has not been built yet. A nested
`cargo build` inside the tests was ruled out on principle: the outer
`cargo test` holds the target-directory lock for its whole run, so a
nested cargo invocation would deadlock.

## 11. Verification

- `crates/omni-jit/tests/jit.rs`:
  - *Correctness*: the safe subset against hand-built and lowered chunks;
    static-depth accounting; `Pop`/`Ret` edge cases.
  - *Differential*: for a corpus of sources (arithmetic, precedence, unary
    minus, `%`, `==`/`!=`, multi-statement programs, NaN and infinity
    propagation, `1 / 0`, `7 % 0`, undefined variables, calls, strings,
    imports), `omni_jit::exec_with` and `Engine::exec` return bit-identical
    values **or** character-identical errors, with optimization on and off.
  - *Throughput*: a 2,000-term constant chain compiled raw; the same chunk
    runs on the JIT and on the VM (`Engine::run_compiled`); the JIT must be
    strictly faster by a real margin (measured in the dev profile, like the
    optimizer's performance tests), and the test prints a greppable
    `JIT: … ns/run vs VM: … ns/run (…x)` line.
  - *aarch64*: compile-only, non-empty buffer for an explicit aarch64 triple.
- `crates/omni-ffi/tests/plugin.rs`:
  - *Manifest verification* (pure Rust, no DLLs): every rejection arm —
    magic, size, abi_version, omni_abi, name rules, count cap, null and
    over-arity functions.
  - *Integration*: load the fixture through `plugin::Plugin`, register on a
    fresh engine, `pluginfixture.double(21) == 42` and
    `pluginfixture.add(1, 2) == 3`; `pluginfixture.crash()` raises a
    positioned `ErrorKind::Host` error containing "panicked" and the engine
    still executes afterwards; loading a path with no manifest symbol fails
    naming `omni_plugin_manifest`.
  - *FFI surface*: `omni_engine_load_plugin` through the C export — success,
    then `omni_exec` of a plugin call returns the value; a bogus path returns
    `OMNI_ERR_ARG`.
- **Row 38** of `scripts/verify-all.sh` builds the fixture cdylib, runs the
  JIT suite with `--nocapture` (capturing the speedup line into the row
  text), runs the plugin suite, and then the cross-host differential:
  Python and Node each evaluate the same 2,000-term sum the Rust suite
  benches and must both answer the known value — the same bytes on every
  host, SDKs untouched at 140/134 suite checks.

## 12. Deliberately deferred

- **Tiering thresholds** ("hot" detection) — needs loops or repeated-call
  sites; the tier compiles every safe chunk eagerly because compilation is
  one Cranelift function and there is nothing to count yet.
- **JIT for strings and calls** — needs a non-trivial ABI between JIT'd code
  and the arena/host-call machinery; the deopt seam is already the place it
  will plug in.
- **Profile and gas accounting for JIT'd instructions** — see §6.
- **SDK bindings for `omni_engine_load_plugin`** — additive surface for the
  SDKs to wrap in a later phase; row 38 proves the SDKs are regression-free
  without them.
- **Plugin function namespacing** (`plugin.fn` dotted names) — flat names
  match the host-call registry today; the manifest carries the plugin name
  so the dotted form is a registration change, not an ABI change.
