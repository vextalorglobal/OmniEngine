# Releasing OmniEngine

One tag produces every artefact: native archives for five platforms, one Python
wheel per platform, one npm tarball carrying all five binaries, and a GitHub
Release. Nothing is compiled on a user's machine at install time.

This document is the runbook for cutting that release, plus the decisions behind
it. The short version:

```bash
python scripts/check-versions.py          # one version, one ABI
./scripts/verify-all.sh                   # 272 Rust, 122 Python, 116 Node checks
./scripts/bench.sh                        # measured numbers, not regressions
git tag v0.1.0 && git push origin v0.1.0  # the only release action
```

## 1. What ships

| Artefact | Platforms | Built by |
| --- | --- | --- |
| `omniengine-0.1.0-<target>.zip` / `.tar.gz` + `.sha256` | win x64 (MSVC), linux x64/arm64 (glibc 2.39), macos x64/arm64 | `scripts/package.py` |
| `omniengine-0.1.0-py3-none-<platform>.whl` | `win_amd64`, `manylinux_2_39_{x86_64,aarch64}`, `macosx_{10_12_x86_64,11_0_arm64}` | `sdks/python/setup.py` in CI |
| `omniengine-0.1.0.tgz` | one tarball, five `native/` directories | `npm pack` after staging |
| `omni-<platform>[.exe]`, `omniup-<platform>[.exe]` + `.sha256` | the same five platforms | `scripts/cli-binaries.py` |
| `omni-update.json` | one manifest naming every platform's URL and digest | `scripts/make-manifest.py` |
| GitHub Release | all of the above | `gh release create` |

The native archive contains the shared library, its import library where the
platform has one, `include/omni.h`, `examples/c/embed.c`, `README.md` and
`LICENSE` — six files, no sources, no `target/` tree.

A wheel contains `omni/_native/<lib>`; an npm tarball contains
`native/<platform>-<arch>/<lib>`. Both are built from the tagged commit in the
same workflow run, so the wrapper and the engine cannot be from different
versions.

The two command-line binaries are the update channel. `omni-update.json` is what
`omni update` fetches from
`https://github.com/fahim5536/omniengine/releases/latest/download/omni-update.json`;
it names a URL and a SHA-256 for each platform, and the installer refuses bytes
that do not match. Because it lives at the `/latest/` path, publishing a release
is what starts offering it — there is no server to deploy.

## 2. Decisions, including the one that changed

**Release packages bundle the engine; checkouts do not.** Until this round both
READMEs argued that shipping a binary inside a wrapper risks a stale engine that
no test catches. That risk is real for a *developer's* `target/debug` copy, and
it is why neither package has ever read one. It does not apply to an artifact
built by CI from the tag that is being released: the wheel and the `.dll` are
produced in the same run, and the install checks the ABI before you can use it.
Refusing to bundle would make `pip install omniengine` and `npm install
omniengine` unusable without a Rust toolchain, which is the opposite of a 60
second start.

**No sdist on PyPI.** The Python project lives in `sdks/python` but the engine
lives in the workspace at the repository root. An sdist of the subdirectory
would either omit the Rust sources (so `pip install` from it cannot build) or
duplicate the workspace. Platform wheels are the supported path. A source
install from a checkout still works: `pip install -e sdks/python` finds
`target/` at run time.

**`py3-none-<platform>`, not `cp3xx-cp3xx-<platform>`.** The binding is
`ctypes`; there is no CPython C extension. `python -m wheel tags` retags the
platform wheel after the build so one wheel serves every Python 3 on that
platform, and it is built once per release.

**Linux wheels are `manylinux_2_39`** because the engine is built on Ubuntu
24.04. That is honest, not aspirational. `auditwheel repair` is *not* run: the
wheel contains one Rust `cdylib` and no bundled third-party `.so`, so the tag
states the glibc floor of the runner rather than the result of a full manylinux
audit. To support older glibc, build the same artifact inside
`quay.io/pypa/manylinux_2_28_*` and change the tag in the workflow; nothing else
moves. macOS floors are Rust's defaults: 10.12 for x64 and 11.0 for arm64.

**CI overrides `rust-toolchain.toml`.** The committed file pins a Windows-GNU
toolchain for the author's machine, where Git's `link.exe` shadows MSVC. The
workflow sets `RUSTUP_TOOLCHAIN=stable`, so Linux and macOS runners use their
native toolchain and Windows uses MSVC rather than MinGW.

## 3. Preflight

Run these from a clean checkout of the tag's commit:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build -p omni-ffi
python sdks/python/tests/test_ffi.py
(cd sdks/nodejs && npm install koffi && npm test)
./scripts/verify-all.sh
python scripts/check-versions.py --tag v0.1.0
python -m twine check dist/*.whl          # after building the wheel
```

`check-versions.py` reads the workspace version from `Cargo.toml`, `[project]`
from `pyproject.toml`, `version` from `package.json`, and the ABI from the C
header, both SDK constants, and the CLI updater's own copy. Any disagreement exits non-zero. The same checks
run in `sdks/nodejs/scripts/prepack.js`, so `npm publish` cannot proceed on a
disagreeing tree even if the workflow is bypassed.

## 4. Versions and ABI

- One version across all three manifests. The release workflow fails otherwise.
- The ABI level changes only when a struct layout or a signature changes. Bumps
  so far: v2 strings and the 16-byte `OmniValue`, v3 host callbacks, v4 batch
  execution and `OmniStats.execs` counting items.
- A version bump does **not** imply an ABI bump. A 0.1.1 that fixes a wrapper
  keeps ABI 4.
- Both SDKs refuse to load an engine whose ABI differs from their own constant.
  A stale artifact is an error, not wrong numbers.

## 5. Configuration that lives outside the repository

| Where | What | Why |
| --- | --- | --- |
| PyPI → Publishing → Trusted publishers | repository `fahim5536/OmniEngine`, workflow `release.yml`, environment `pypi` | OIDC, no API token stored |
| GitHub → Environments | `pypi` | matches the trusted publisher |
| GitHub → Secrets | `NPM_TOKEN` (npm automation token) | npm does not support OIDC publishing for first-party packages |
| Repository settings | Actions allowed to create releases (`contents: write` is granted per job) | `gh release create` uses `GITHUB_TOKEN` |

If `NPM_TOKEN` is absent, the npm publish step fails the release. Build the
tarball anyway with `workflow_dispatch` (which never publishes) or remove the
publish step for a dry run.

## 6. Cutting the release

```bash
# on main, with a clean tree
python scripts/check-versions.py
git tag -a v0.1.0 -m "OmniEngine 0.1.0"
git push origin v0.1.0
```

The tag push runs the workflow. Jobs, in order:

1. **guard** — versions and ABI agree, tag matches the manifests.
2. **verify** — fmt, clippy, 272 Rust tests, both SDK suites.
3. **native** — five platforms, in parallel; each archive is smoke-tested with
   `omni run examples/hello.omni` before upload, and the same job stages that
   platform's `omni` and `omniup` with their checksums.
4. **update-manifest** — waits for all five binaries, recomputes every digest,
   and writes `omni-update.json` for the tagged version.
5. **python-wheels** — one wheel per platform, engine staged from that
   platform's build, retagged `py3-none-<platform>`.
6. **npm-package** — all five libraries staged under `native/`, `prepack.js`
   enforces completeness, `npm pack`, then `npm publish --provenance`.
7. **pypi** — Trusted Publishing uploads the wheels.
8. **github-release** — archives, wheels, tarball, both CLI binaries per
   platform, the update manifest, and checksums attached.

Re-running a failed tag build is safe: the GitHub Release step updates an
existing release with `--clobber`, while PyPI and npm reject a version that
already exists. If a publish job failed after a partial upload, fix the cause and
re-run only that job; npm and PyPI are idempotent at the version level (both
refuse duplicates rather than replacing them).

## 7. Post-release verification (do this, do not assume)

From a directory outside the repository, with no `OMNI_*` variables set:

```bash
python -m venv fresh && . fresh/bin/activate      # Scripts/activate on Windows
pip install omniengine
python -c "import omni; print(omni.Engine().evaluate('40 + 2'))"   # 42.0

mkdir fresh-npm && cd fresh-npm && npm init -y
npm install omniengine koffi
node -e "const {Engine}=require('omniengine'); const e=new Engine(); console.log(e.evaluate('40 + 2')); e.close()"
```

Then check the installed layout itself:

```bash
python -c "import omni, os; print(os.path.dirname(omni.__file__))"
# omni/_native/omni.dll (or libomni.so / libomni.dylib) must sit there
ls node_modules/omniengine/native/*/
```

Then the update channel, from the same outside directory:

```bash
omni --version            # omni 0.1.0 (language v2, ABI 4)
omni check-update; echo $?  # 0 -- the release you just published is current
omni update --dry-run; echo $?  # 10 only when a *later* release exists
```

`check-update` returning 10 immediately after a release means the manifest names
a version the binaries do not; 2 means the manifest was not reachable or does not
parse. Both are release bugs and are fixed by republishing, not by editing a
client.

The npm install prints `omniengine: bundled engine verified (<platform>, ABI 4)`
when postinstall succeeds. `OMNI_SKIP_POSTINSTALL=1` skips it;
`OMNI_STRICT_POSTINSTALL=1` turns a missing artifact into a failed install.

Finally, download one native archive, compile `examples/c/embed.c` against it,
and confirm `all C embedding checks passed`. That is the only check that
exercises the header, the import library, and the DLL together.

## 8. Rollback

- **PyPI**: `yank` the release from the project page. The files stay for
  existing pins; new installs do not see them.
- **npm**: `npm deprecate omniengine@0.1.0 "reason"`. Deleting a version is
  allowed only within 72 hours and breaks anyone who pinned it; deprecation is
  the honest default.
- **GitHub**: mark the release as a pre-release or delete it. Tags are immutable
  here: fixing a bad release means a new patch version, never a force-push.
- **The update feed**: `/latest/download/omni-update.json` follows whichever
  release is marked latest. A bad manifest is corrected by publishing the next
  patch version; a manifest that should not be served at all can be removed from
  the release, which makes clients exit 2 (check failed) instead of installing
  anything. Clients verify the digest before replacing a byte, so a truncated or
  corrupted download is a refusal, not a broken install.

## 9. Known limits of this release

- No sdist (see §2).
- Linux wheels require glibc 2.39 unless the build is moved into a manylinux
  container.
- Windows wheels are x64 only; arm64 Windows is not in the matrix.
- The Windows C archive is MSVC-built (`lib/omni.dll.lib`). `examples/c/embed.c`
  was compiled and run here against the GNU archive (`lib/libomni.dll.a`), not
  against an MSVC import library — no MSVC C session was run from this machine.
- The npm tarball carries five engines. Measured with the Windows engine only,
  it is 536.5 kB packed / 1.4 MB unpacked; the four cross-built libraries add
  unknown bytes to that, and no five-platform number is claimed from this
  machine. Splitting into per-platform optional packages would make installs
  smaller; it also makes five packages to version and publish. One tarball is
  the simpler honest trade-off at 0.1.0.
- Update authenticity rests on TLS to GitHub plus the digest inside the
  manifest: the SHA-256 catches corruption and a swapped asset, not a
  compromised release host, and the manifest is not separately signed.
- The workflow itself has been syntax-checked and every step it calls has been
  run locally on Windows, but a GitHub-hosted run has not been performed from
  this machine (no remote is configured here). The first tag push is the first
  full run.
