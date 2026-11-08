//! Integration tests for the JIT tier: differential parity against the VM,
//! the deopt-to-VM fallback with positional error accuracy, a throughput
//! comparison on a long straight-line sum, and cross-target compilation.

use std::time::Instant;

use omni_core::ast::Ast;
use omni_core::bytecode::{self, Chunk};
use omni_core::{Engine, ErrorKind, OmniError, Value, lex_into};
use omni_jit::{Deopt, Jit, compile_bytes, exec_with};

/// Programs whose result both tiers must agree on, bit for bit, error for
/// error. Every string-valued result is excluded: those live in the arena,
/// so two runs hold different pointers even when the text is identical.
const CORPUS: &[&str] = &[
    "",
    "1",
    "1 + 1",
    "2 * 21",
    "7 - 10",
    "3 / 2",
    "-5",
    "--5",
    "1 == 1",
    "1 != 1",
    "1 == 2",
    "nil",
    "undef",
    "true",
    "false",
    "(1 + 2) * (3 + 4)",
    "0.0 == -0.0",
    "1e308 * 10",
    "1e308 * 10 - 1e308 * 10",
    "1 % 2",
    "let x = 1\nx + 2",
    "len(\"abc\")",
    "1 / 0",
    "nil + 1",
    "1 +",
    "not a program",
    "@#$%",
];

fn describe(result: Result<Value, OmniError>) -> String {
    match result {
        Ok(value) => format!("ok:{:016x}", value.bits()),
        Err(error) => format!(
            "err:{:?}:{}:{}:{}",
            error.kind, error.line, error.column, error.message
        ),
    }
}

#[test]
fn every_corpus_program_matches_the_vm_with_and_without_the_optimizer() {
    for source in CORPUS {
        for optimize in [false, true] {
            let mut vm_engine = Engine::new();
            vm_engine.set_optimize(optimize);
            let vm = vm_engine.exec(source);

            let mut tier_engine = Engine::new();
            let tier = exec_with(&mut tier_engine, source, optimize);

            assert_eq!(
                describe(vm),
                describe(tier),
                "source={source:?} optimize={optimize}"
            );
        }
    }
}

#[test]
fn a_deopt_reruns_the_same_chunk_through_the_vm_with_its_position() {
    // A JIT-safe chunk that divides by zero: the native code deopts, and the
    // tier's door hands back the VM's canonical positioned error.
    let source = "1 / 0";
    let mut tokens = Vec::new();
    lex_into(source, &mut tokens).unwrap();
    let mut ast = Ast::new();
    ast.parse_into(source, &tokens).unwrap();
    let mut engine = Engine::new();
    let mut chunk = Chunk::new();
    let mut tasks = Vec::new();
    bytecode::compile(&ast, source, &engine, &mut chunk, &mut tasks).unwrap();

    let jit = Jit::compile(&chunk, source).expect("1 / 0 is in the JIT-safe subset");
    assert_eq!(jit.run(), Err(Deopt));

    let error = exec_with(&mut engine, source, false).unwrap_err();
    assert_eq!(error.kind, ErrorKind::Runtime);
    assert_eq!(error.message, "division by zero");
    assert!(error.line >= 1, "line must be 1-based: {error}");
    assert!(error.column >= 1, "column must be 1-based: {error}");
}

fn build_chunk(source: &str) -> Chunk {
    let mut tokens = Vec::new();
    lex_into(source, &mut tokens).unwrap();
    let mut ast = Ast::new();
    ast.parse_into(source, &tokens).unwrap();
    let engine = Engine::new();
    let mut chunk = Chunk::new();
    let mut tasks = Vec::new();
    bytecode::compile(&ast, source, &engine, &mut chunk, &mut tasks).unwrap();
    chunk
}

/// The 38th verification row greps for the `JIT: … ns/run vs VM: … ns/run`
/// line this test prints, so its format is load-bearing.
#[test]
fn the_jit_outruns_the_vm_on_a_long_straight_line_sum() {
    // 2,000 terms, no optimizer: the chunk stays 2,000 pushes and 1,999 adds,
    // so the comparison is native arithmetic against VM dispatch.
    let source = std::iter::repeat_n("1", 2_000)
        .collect::<Vec<_>>()
        .join(" + ");
    let chunk = build_chunk(&source);
    let jit = Jit::compile(&chunk, &source).expect("the sum must be JIT-safe");
    let mut engine = Engine::new();

    let sum: f64 = jit.run().unwrap().as_f64().unwrap();
    let vm_sum: f64 = engine
        .run_compiled(&chunk, &source)
        .unwrap()
        .as_f64()
        .unwrap();
    assert_eq!(sum, 2_000.0);
    assert_eq!(vm_sum, 2_000.0);

    const WARMUP: u32 = 20;
    const ITERATIONS: u32 = 500;
    for _ in 0..WARMUP {
        let _ = std::hint::black_box(jit.run());
        let _ = std::hint::black_box(engine.run_compiled(&chunk, &source));
    }

    let start = Instant::now();
    for _ in 0..ITERATIONS {
        std::hint::black_box(jit.run().unwrap());
    }
    let jit_ns = start.elapsed().as_nanos() / u128::from(ITERATIONS);

    let start = Instant::now();
    for _ in 0..ITERATIONS {
        std::hint::black_box(engine.run_compiled(&chunk, &source).unwrap());
    }
    let vm_ns = start.elapsed().as_nanos() / u128::from(ITERATIONS);

    println!(
        "JIT: {} ns/run vs VM: {} ns/run ({:.2}x)",
        jit_ns,
        vm_ns,
        vm_ns as f64 / jit_ns as f64
    );
    assert!(
        jit_ns < vm_ns,
        "the JIT must beat the VM on a straight-line sum: {jit_ns}ns vs {vm_ns}ns"
    );
}

#[test]
fn the_tier_compiles_for_aarch64() {
    let triple = "aarch64-unknown-linux-gnu".parse().unwrap();
    let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
    let isa = cranelift_codegen::isa::lookup(triple)
        .expect("aarch64 must be a known target")
        .finish(flags)
        .expect("aarch64 isa must build");

    let source = "21 2";
    let chunk = build_chunk(source);
    let bytes = compile_bytes(&chunk, source, &*isa).expect("the chunk must compile for aarch64");
    assert!(!bytes.is_empty(), "aarch64 lowering must emit machine code");
}

#[test]
fn an_unsafe_chunk_falls_back_instead_of_compiling() {
    // `%` is outside the JIT-safe subset (no frem in Cranelift 0.121), so
    // the door runs the VM and the result is still exact.
    let mut engine = Engine::new();
    let value = exec_with(&mut engine, "5 % 3", false).unwrap();
    assert_eq!(value.as_f64(), Some(2.0));

    let chunk = build_chunk("5 % 3");
    assert!(Jit::compile(&chunk, "5 % 3").is_none());
}
