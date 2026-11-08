# OmniEngine fuzz targets

Four libFuzzer targets over the pipeline:

| Target | Entry point | Reaches |
| --- | --- | --- |
| `fuzz_lex` | `fuzz_api::lex_only` | lexer |
| `fuzz_parse` | `fuzz_api::parse_only` | lexer + AST parser |
| `fuzz_lower` | `fuzz_api::lower_only` | lexer + parser + bytecode lowering |
| `fuzz_vm` | `fuzz_api::execute`, `execute_bytes` | whole engine + raw instruction decoder |

This directory is its own workspace. It is deliberately **not** a member of the
root workspace, so `cargo build`, `cargo test`, and `cargo clippy` on the pinned
stable toolchain never resolve `libfuzzer-sys` or require nightly.

## libFuzzer

```bash
rustup toolchain install nightly
cargo install cargo-fuzz

cd crates/omni-core/fuzz
cargo +nightly fuzz run fuzz_parse corpus/fuzz_parse -- -max_total_time=60
```

Replace `fuzz_parse` with any target from the table. A crash is written to
`artifacts/` and the input that caused it to `corpus/<target>/crash-*`; both are
git-ignored. Reproduce with:

```bash
cargo +nightly fuzz run fuzz_vm corpus/fuzz_vm/crash-<hash>
```

## Deterministic harness

libFuzzer needs nightly. For machines that only have the repository's pinned
stable toolchain, the crate ships a dependency-free mutation harness as a test:

```bash
bash scripts/fuzz.sh --deterministic
```

It mutates a fixed corpus with a seeded xorshift64 PRNG, runs every input
through all four stages under `catch_unwind`, and asserts no panic, a valid
`[Line L, Col C]` on every error, and deterministic results. `verify-all.sh`
runs this as its fuzz row. `scripts/fuzz.sh` without a flag prefers `cargo-fuzz`
and falls back to the harness instead of installing anything.

## Invariants the targets are looking for

- No panic, abort, or out-of-bounds access on any input, valid UTF-8 or not
  (the public API takes `&str`; the target converts lossily).
- Every rejection is a positioned `OmniError`.
- The VM stays bounded: the step budget stops backward jumps, and the stack cap
  stops push storms, even for chunks built from arbitrary bytes.
- The fs sandbox refuses paths that canonicalize outside an allowed root,
  including dangling symlinks.
