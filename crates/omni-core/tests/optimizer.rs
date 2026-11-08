//! Optimizer contract tests for `omni-core`.
//!
//! External tests on purpose: they use only the public API, so they prove what
//! an embedder observes — values, errors, gas, and the profile — rather than
//! what the bytecode happens to contain. The claims under test are the ones in
//! the Phase 9 spec:
//!
//! 1. constant arithmetic folds into one push, with float semantics preserved,
//! 2. dead statements disappear but every effect survives,
//! 3. optimized and unoptimized runs fail identically, position included,
//! 4. optimized code dispatches fewer instructions and runs faster.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use omni_core::{Engine, ErrorKind, HostOutcome, OPCODE_COUNT, Value, opcode_name};

fn num(value: f64) -> Value {
    Value::from_f64(value)
}

/// The discriminant of the opcode with the given mnemonic, so tests name
/// opcodes by contract rather than by table position.
fn index_of(mnemonic: &str) -> usize {
    (0..OPCODE_COUNT)
        .find(|&index| opcode_name(index as u32) == Some(mnemonic))
        .expect("every mnemonic used by these tests exists")
}

fn calls(engine: &Engine, mnemonic: &str) -> u64 {
    engine.profile().opcode_calls[index_of(mnemonic)]
}

#[test]
fn constant_arithmetic_folds_into_one_push() {
    let mut engine = Engine::new();
    assert_eq!(engine.exec("2 + 3"), Ok(num(5.0)));

    let optimize = engine.last_optimize();
    assert_eq!(optimize.folded, 1, "one Add folded");
    assert_eq!(optimize.elided, 0, "nothing dead here");

    let profile = engine.profile();
    assert_eq!(profile.total_instructions, 2, "PUSH_FOLDED then RET");
    assert_eq!(calls(&engine, "ADD"), 0);
    assert_eq!(calls(&engine, "PUSH_NUMBER"), 0);
    assert_eq!(calls(&engine, "PUSH_FOLDED"), 1);
}

#[test]
fn folding_respects_precedence_and_float_arithmetic() {
    let mut engine = Engine::new();
    // Mul folds first, then the Add over its result: two folds, one push.
    assert_eq!(engine.exec("2 + 3 * 4"), Ok(num(14.0)));
    assert_eq!(engine.last_optimize().folded, 2);
    assert_eq!(calls(&engine, "PUSH_FOLDED"), 1);

    assert_eq!(engine.exec("7 / 2"), Ok(num(3.5)));
    assert_eq!(engine.last_optimize().folded, 1);

    // Unary negation folds over the folded sum.
    assert_eq!(engine.exec("-(2 + 3)"), Ok(num(-5.0)));
    assert_eq!(engine.last_optimize().folded, 2);
    assert_eq!(calls(&engine, "NEG"), 0);

    // The Phase 9 verification row program.
    assert_eq!(engine.exec("(2 + 3) * 4 - 8 / 2"), Ok(num(16.0)));
    assert_eq!(engine.last_optimize().folded, 4);
    for mnemonic in ["ADD", "SUB", "MUL", "DIV"] {
        assert_eq!(calls(&engine, mnemonic), 0, "{mnemonic} must not run");
    }
}

#[test]
fn division_and_remainder_by_zero_survive_to_runtime() {
    let mut optimized = Engine::new();
    let mut raw = Engine::new();
    raw.set_optimize(false);

    for source in ["1 / 0", "1 % 0", "(2 + 3) / 0", "let x = 1\nx / 0"] {
        let folded = optimized.exec(source).unwrap_err();
        let unfolded = raw.exec(source).unwrap_err();
        assert_eq!(
            folded, unfolded,
            "{source:?}: optimizing must not change the failure"
        );
    }

    assert_eq!(
        optimized.exec("1 / 0").unwrap_err().kind,
        ErrorKind::Runtime
    );
    assert_eq!(
        optimized.exec("1 / 0").unwrap_err().message,
        "division by zero"
    );
    assert_eq!(
        optimized.exec("1 % 0").unwrap_err().message,
        "remainder by zero"
    );

    // The folded dividend disappears, the division by the constant zero does
    // not: exactly one fold, and the runtime error keeps the operator position.
    let error = optimized.exec("(2 + 3) / 0").unwrap_err();
    assert_eq!(optimized.last_optimize().folded, 1);
    assert_eq!((error.line, error.column), (1, 9));

    let error = optimized.exec("let x = 1\nx / 0").unwrap_err();
    assert_eq!((error.line, error.column), (2, 3));
}

#[test]
fn dead_statements_are_eliminated() {
    let mut engine = Engine::new();
    assert_eq!(engine.exec("5\n2 + 3"), Ok(num(5.0)));

    let optimize = engine.last_optimize();
    assert_eq!(optimize.folded, 1, "the tail expression folded");
    assert_eq!(optimize.elided, 2, "PUSH_NUMBER 5 and its POP vanished");

    let profile = engine.profile();
    assert_eq!(profile.total_instructions, 2, "PUSH_FOLDED then RET");
    assert_eq!(calls(&engine, "PUSH_NUMBER"), 0);
    assert_eq!(calls(&engine, "POP"), 0);
}

#[test]
fn effects_are_never_elided() {
    let mut engine = Engine::new();

    // A let stores: the statement is a side effect and must survive. The
    // binding is visible to the next statement in the same execution.
    assert_eq!(engine.exec("let x = 9\nx + 1"), Ok(num(10.0)));
    assert_eq!(engine.last_optimize().elided, 0);
    assert_eq!(calls(&engine, "STORE"), 1);

    // A call runs the host function: folding may shrink its arguments, but
    // the call itself executes.
    let counter = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&counter);
    engine
        .register_host_fn("bump", 1, move |_engine, args| {
            observed.fetch_add(1, Ordering::Relaxed);
            HostOutcome::Number(args[0].as_f64().unwrap_or(0.0) + 1.0)
        })
        .unwrap();
    assert_eq!(engine.exec("bump(1 + 1)\n2 + 3"), Ok(num(5.0)));
    assert_eq!(counter.load(Ordering::Relaxed), 1);
    assert_eq!(calls(&engine, "CALL"), 1);

    // An import binds a module namespace within the execution: it must run.
    // If DCE dropped it, the following statement would not resolve `m.answer`.
    let root = std::env::temp_dir().join(format!(
        "omni-optimizer-modules-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("m.omni"), "let answer = 40 + 2").unwrap();
    engine.set_module_root(&root).unwrap();

    assert_eq!(engine.exec("import \"m\"\nm.answer"), Ok(num(42.0)));
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn invalid_escapes_are_never_elided() {
    let mut optimized = Engine::new();
    let mut raw = Engine::new();
    raw.set_optimize(false);

    // The statement is a candidate for DCE, but a literal that cannot decode
    // keeps its failure mode, so the error must fire either way.
    let folded = optimized.exec("\"\\q\"\n1 + 2").unwrap_err();
    let unfolded = raw.exec("\"\\q\"\n1 + 2").unwrap_err();
    assert_eq!(folded, unfolded);
    assert!(folded.message.contains("unknown escape '\\q'"), "{folded}");
}

#[test]
fn optimized_execution_dispatches_fewer_instructions() {
    let source = format!("1{}", " + 1".repeat(10_000));

    let mut optimized = Engine::new();
    assert_eq!(optimized.exec(&source), Ok(num(10_001.0)));
    let folded_total = optimized.profile().total_instructions;
    assert_eq!(folded_total, 2, "the whole spine collapses to one push");

    let mut raw = Engine::new();
    raw.set_optimize(false);
    assert_eq!(raw.exec(&source), Ok(num(10_001.0)));
    let raw_total = raw.profile().total_instructions;
    assert_eq!(raw_total, 20_002, "push per term, add per term, one RET");

    assert!(
        folded_total * 100 < raw_total,
        "optimized code must dispatch a small fraction of the instructions"
    );
}

#[test]
fn dead_strings_make_repeated_executions_faster() {
    // A dead 40 KiB string: unoptimized execution decodes and allocates it
    // every run; optimized execution never touches it. The gap is allocation
    // work, not scheduler noise, so summing many runs is a fair race.
    let source = format!("\"{}\"; 2 + 3", "x".repeat(40 * 1024));
    const RUNS: usize = 500;

    let mut optimized = Engine::new();
    let mut raw = Engine::new();
    raw.set_optimize(false);

    assert_eq!(optimized.exec(&source), Ok(num(5.0)));
    assert_eq!(optimized.last_optimize().elided, 2);
    assert_eq!(
        optimized.profile().peak_bytes,
        0,
        "the string was never built"
    );

    assert_eq!(raw.exec(&source), Ok(num(5.0)));
    assert!(
        raw.profile().peak_bytes >= 40 * 1024,
        "the string was built"
    );

    let mut folded_ns = 0u64;
    let mut raw_ns = 0u64;
    for _ in 0..RUNS {
        optimized.exec(&source).unwrap();
        folded_ns = folded_ns.saturating_add(optimized.profile().elapsed_ns);
        raw.exec(&source).unwrap();
        raw_ns = raw_ns.saturating_add(raw.profile().elapsed_ns);
    }
    assert!(
        folded_ns < raw_ns,
        "optimized {folded_ns}ns must beat unoptimized {raw_ns}ns over {RUNS} runs"
    );
}

#[test]
fn folding_reduces_gas_consumption() {
    let mut optimized = Engine::new();
    optimized.set_step_limit(4);
    assert_eq!(optimized.exec("1 + 1 + 1"), Ok(num(3.0)));

    let mut raw = Engine::new();
    raw.set_step_limit(4);
    raw.set_optimize(false);
    let error = raw.exec("1 + 1 + 1").unwrap_err();
    assert_eq!(error.kind, ErrorKind::Runtime);
    assert!(
        error.message.contains("step limit of 4"),
        "unexpected message: {}",
        error.message
    );
}

#[test]
fn optimizer_stats_reset_and_respect_the_switch() {
    let mut engine = Engine::new();
    assert!(engine.optimize(), "the optimizer is on by default");
    assert!(!engine.profiling(), "profiling is off by default");

    assert_eq!(engine.exec("1 + 2"), Ok(num(3.0)));
    assert_eq!(engine.last_optimize().folded, 1);

    // A program with nothing foldable resets the per-execution counters.
    assert_eq!(engine.exec("let a = 1\na + a"), Ok(num(2.0)));
    assert_eq!(engine.last_optimize().folded, 0);
    assert_eq!(engine.last_optimize().elided, 0);

    engine.set_optimize(false);
    assert!(!engine.optimize());
    assert_eq!(engine.exec("1 + 2"), Ok(num(3.0)));
    let optimize = engine.last_optimize();
    assert_eq!((optimize.folded, optimize.elided), (0, 0));
    assert_eq!(calls(&engine, "ADD"), 1);
    assert_eq!(calls(&engine, "PUSH_FOLDED"), 0);
    assert_eq!(engine.profile().total_instructions, 4);
}

#[test]
fn optimizer_knobs_fork_with_the_engine() {
    let mut parent = Engine::new();
    parent.set_optimize(false);

    let mut child = parent.fork();
    assert!(
        !child.optimize(),
        "the knob is configuration, copied by fork"
    );
    assert_eq!(child.exec("1 + 1"), Ok(num(2.0)));
    assert_eq!(
        (child.last_optimize().folded, child.last_optimize().elided),
        (0, 0)
    );

    child.set_optimize(true);
    assert_eq!(child.exec("1 + 1"), Ok(num(2.0)));
    assert_eq!(child.last_optimize().folded, 1);

    // The parent's knob is its own.
    assert!(!parent.optimize());
}
