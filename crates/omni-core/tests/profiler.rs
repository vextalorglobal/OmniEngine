//! Profiler contract tests for `omni-core`.
//!
//! External tests, public API only. The claims under test are the Phase 9
//! spec's: the heatmap always balances the instruction count, per-opcode
//! self-times exist only when profiling is enabled, `peak_bytes` bounds what
//! an execution really allocated, and every counter is per-execution — a
//! batch item or a fork never inherits another run's profile.

use omni_core::{Engine, OPCODE_COUNT, Value, opcode_name};

fn num(value: f64) -> Value {
    Value::from_f64(value)
}

fn index_of(mnemonic: &str) -> usize {
    (0..OPCODE_COUNT)
        .find(|&index| opcode_name(index as u32) == Some(mnemonic))
        .expect("every mnemonic used by these tests exists")
}

fn calls(engine: &Engine, mnemonic: &str) -> u64 {
    engine.profile().opcode_calls[index_of(mnemonic)]
}

#[test]
fn the_opcode_heatmap_sums_to_the_instruction_count() {
    let mut engine = Engine::new();
    assert_eq!(
        engine.exec("let x = 2\nx * 3 + len(\"abcd\")"),
        Ok(num(10.0))
    );

    let profile = engine.profile();
    let heatmap_sum: u64 = profile.opcode_calls.iter().sum();
    assert_eq!(profile.total_instructions, heatmap_sum);
    assert_eq!(heatmap_sum, profile.opcode_calls.iter().sum::<u64>());
    assert!(
        profile.total_instructions >= 7,
        "a mixed program runs several ops"
    );

    // Spot checks by name, so the heatmap is proven addressable the way a
    // host will read it — through `opcode_name` indices.
    assert!(calls(&engine, "LOAD") >= 1, "x is loaded at least once");
    assert!(calls(&engine, "CALL") >= 1, "len is called");
    assert!(calls(&engine, "PUSH_STR") >= 1, "the literal is pushed");
    assert!(calls(&engine, "RET") == 1, "one chunk, one return");
}

#[test]
fn per_opcode_timing_stays_zero_until_enabled() {
    let mut engine = Engine::new();
    assert!(!engine.profiling(), "off by default");
    assert_eq!(engine.exec("1 + 2"), Ok(num(3.0)));

    let untimed = engine.profile();
    assert!(untimed.total_instructions > 0);
    assert_eq!(
        untimed.opcode_ns.iter().sum::<u64>(),
        0,
        "self-times cost a clock read and must stay off"
    );
    assert!(
        untimed.elapsed_ns > 0,
        "the whole-run wall clock is recorded even without profiling"
    );

    engine.set_profiling(true);
    assert!(engine.profiling());
    let chain = format!("let x = 1\nx{}", " + 1".repeat(1_000));
    assert_eq!(engine.exec(&chain), Ok(num(1_001.0)));

    let timed = engine.profile();
    let self_time: u64 = timed.opcode_ns.iter().sum();
    assert!(self_time > 0, "enabled profiling brackets every dispatch");
    assert!(
        self_time <= timed.elapsed_ns,
        "self-times live inside the whole-run wall clock: {self_time} vs {}",
        timed.elapsed_ns
    );
    assert_eq!(
        timed.opcode_calls.iter().sum::<u64>(),
        timed.total_instructions
    );

    engine.set_profiling(false);
    assert_eq!(engine.exec("1 + 2"), Ok(num(3.0)));
    assert_eq!(engine.profile().opcode_ns.iter().sum::<u64>(), 0);
}

#[test]
fn peak_bytes_bounds_a_string_heavy_program() {
    let mut engine = Engine::new();

    assert_eq!(engine.exec("1 + 2"), Ok(num(3.0)));
    assert_eq!(engine.profile().peak_bytes, 0, "scalars allocate nothing");

    let big = "x".repeat(64 * 1024);
    let source = format!("let big = \"{big}\"\nlen(big)");
    assert_eq!(engine.exec(&source), Ok(num(big.len() as f64)));

    let profile = engine.profile();
    assert!(
        profile.peak_bytes >= big.len() as u64,
        "the literal must be materialised: {} >= {}",
        profile.peak_bytes,
        big.len()
    );
    assert!(
        profile.peak_bytes < 2 * big.len() as u64,
        "nothing else in this program is large: {}",
        profile.peak_bytes
    );
}

#[test]
fn the_profile_is_per_execution_and_never_forked() {
    let mut parent = Engine::new();
    parent.set_profiling(true);

    // A heavier run, then a tiny one: the profile describes the last one.
    let chain = format!("let x = 1\nx{}", " + 1".repeat(100));
    parent.exec(&chain).unwrap();
    let heavy_total = parent.profile().total_instructions;
    assert!(heavy_total > 200);

    assert_eq!(parent.exec("7"), Ok(num(7.0)));
    let light = parent.profile();
    assert_eq!(light.total_instructions, 2, "PUSH_NUMBER then RET");
    assert_eq!(calls(&parent, "PUSH_NUMBER"), 1);

    // A fork copies the knob but none of the execution history.
    let child = parent.fork();
    assert!(child.profiling(), "profiling is configuration");
    let fresh = child.profile();
    assert_eq!(fresh.total_instructions, 0);
    assert_eq!(fresh.opcode_calls.iter().sum::<u64>(), 0);
    assert_eq!(fresh.peak_bytes, 0);
    assert_eq!(fresh.elapsed_ns, 0);
}

#[test]
fn a_failed_execution_is_profiled_like_a_successful_one() {
    let mut engine = Engine::new();
    engine.set_profiling(true);
    assert!(engine.exec("1 / 0").is_err());

    let profile = engine.profile();
    assert_eq!(
        profile.total_instructions, 3,
        "two pushes, then the DIV fails"
    );
    assert_eq!(calls(&engine, "DIV"), 1);
    assert!(
        profile.opcode_ns.iter().sum::<u64>() > 0,
        "timing survived the error"
    );
}

#[test]
fn exec_many_profiles_each_item_and_reports_the_last() {
    let mut engine = Engine::new();
    let sources = ["1 + 2", "7", "2 * 21"];
    let results = engine.exec_many(&sources);
    assert_eq!(results[0].value.as_f64(), Some(3.0));
    assert_eq!(results[1].value.as_f64(), Some(7.0));
    assert_eq!(results[2].value.as_f64(), Some(42.0));

    // The last item was `2 * 21`, folded to one push: PUSH_FOLDED then RET.
    let profile = engine.profile();
    assert_eq!(profile.total_instructions, 2);
    assert_eq!(calls(&engine, "PUSH_FOLDED"), 1);
    assert_eq!(
        profile.opcode_calls.iter().sum::<u64>(),
        profile.total_instructions
    );
}
