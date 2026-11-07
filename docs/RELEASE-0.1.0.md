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

**Quality.** 21 of 21 cross-host verification rows pass on this machine: 225 Rust
tests, 107 Python, 93 Node, plus the C embedding example. `cargo fmt --check` and
`cargo clippy -- -D warnings` are clean.

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
