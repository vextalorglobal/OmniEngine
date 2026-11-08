//! Concurrency contract tests for `omni-core`.
//!
//! These are external tests on purpose: they use only the public API, so they
//! prove what an embedder can rely on rather than what the implementation
//! happens to expose. Three claims are under test:
//!
//! 1. an `Engine` is `Send` and `Sync` — it moves to a worker thread, and many
//!    threads may read one at once,
//! 2. `Engine::fork` copies configuration but no execution history, so a worker
//!    starts clean and still sees its host's globals and callbacks,
//! 3. `Engine::exec_parallel` is ordered, per-item gas-bounded, and copies every
//!    result into the parent's egress before it returns.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

use omni_core::{Engine, ErrorKind, HostOutcome, Value};

#[test]
fn sixty_four_threads_run_engines_under_strict_gas() {
    const THREADS: usize = 64;
    let barrier = Arc::new(Barrier::new(THREADS));
    let handles: Vec<_> = (0..THREADS)
        .map(|index| {
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let mut engine = Engine::new();
                engine.set_step_limit(5_000);
                barrier.wait();

                let expected = (index * 3 + 1) as f64;
                let value = engine.exec(&format!("{index} * 3 + 1")).unwrap();
                assert_eq!(value.as_f64(), Some(expected));
                assert_eq!(engine.stats().execs, 1);

                // A long program must fail inside *this* engine's gas, and a
                // good program after it must still run: one exhausted item
                // cannot poison the context. The chain names a variable so the
                // optimizer cannot fold it away — gas must still burn one
                // instruction per term.
                let long = format!("let x = 1\nx{}", " + 1".repeat(6_000));
                let error = engine.exec(&long).unwrap_err();
                assert_eq!(error.kind, ErrorKind::Runtime);
                assert!(
                    error.message.contains("step limit of 5000"),
                    "unexpected message: {}",
                    error.message
                );

                let value = engine.exec(&format!("{index} + 100")).unwrap();
                assert_eq!(value.as_f64(), Some(index as f64 + 100.0));
                assert_eq!(engine.stats().execs, 3);
            })
        })
        .collect();
    for handle in handles {
        handle.join().expect("worker thread panicked");
    }
}

#[test]
fn one_engine_is_shared_read_only_and_forked_by_many_threads() {
    let mut engine = Engine::new();
    let shared = engine.exec("\"shared bytes\"").unwrap();
    let engine = Arc::new(engine);
    let barrier = Arc::new(Barrier::new(12));
    let handles: Vec<_> = (0..12)
        .map(|index| {
            let engine = Arc::clone(&engine);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                // Shared reads: string content, counters, and a fork that reads
                // every piece of configuration without mutating the parent.
                assert_eq!(engine.string_of(shared), Some(&b"shared bytes"[..]));
                assert_eq!(engine.render(shared), "shared bytes");
                assert_eq!(engine.stats().execs, 1);

                let mut child = engine.fork();
                let value = child.exec(&format!("{index} + 1")).unwrap();
                assert_eq!(value.as_f64(), Some(index as f64 + 1.0));
                assert_eq!(child.stats().execs, 1);
            })
        })
        .collect();
    for handle in handles {
        handle.join().expect("reader thread panicked");
    }
    // The parent still owns the only execution it ever ran.
    assert_eq!(engine.stats().execs, 1);
    assert_eq!(engine.string_of(shared), Some(&b"shared bytes"[..]));
}

#[test]
fn a_fork_copies_configuration_but_not_execution_history() {
    let mut parent = Engine::new();
    parent.define("answer", Value::from_f64(42.0));
    parent.define_string("greeting", b"hello").unwrap();
    parent
        .register_host_fn("twice", 1, |_engine, args| {
            HostOutcome::Number(args[0].as_f64().unwrap_or(0.0) * 2.0)
        })
        .unwrap();
    parent.set_step_limit(1_234);
    parent.exec("let private = 9").unwrap();

    let mut child = parent.fork();
    assert_eq!(child.step_limit(), 1_234);
    assert!(child.exec("private").is_err(), "a let is not configuration");
    assert_eq!(child.exec("answer").unwrap().as_f64(), Some(42.0));
    assert_eq!(child.exec("len(greeting)").unwrap().as_f64(), Some(5.0));
    assert_eq!(child.exec("twice(21)").unwrap().as_f64(), Some(42.0));

    // The child owns its copy of the string, so redefining the parent's binding
    // cannot change what the worker sees.
    parent.define_string("greeting", b"goodbye").unwrap();
    assert_eq!(child.exec("len(greeting)").unwrap().as_f64(), Some(5.0));
}

#[test]
fn parallel_batches_preserve_order_and_isolate_failures() {
    let mut engine = Engine::new();
    engine.set_step_limit(400);
    // Names a variable so the optimizer cannot fold the chain into one push.
    let long = format!("let x = 1\nx{}", " + 1".repeat(500));
    let sources = ["1 + 1", "1 / 0", long.as_str(), "\"alpha\"", "2 * 21"];

    let items = engine.exec_parallel(&sources, 4);
    assert_eq!(items.len(), sources.len());

    assert_eq!(items[0].value.as_f64(), Some(2.0));
    assert!(items[0].is_ok());

    let division = items[1].error.as_ref().expect("division must fail");
    assert_eq!(division.kind, ErrorKind::Runtime);
    assert_eq!(division.message, "division by zero");

    let gas = items[2].error.as_ref().expect("the long item must fail");
    assert_eq!(gas.kind, ErrorKind::Runtime);
    assert!(gas.message.contains("step limit"), "{}", gas.message);

    assert_eq!(engine.string_of(items[3].value), Some(&b"alpha"[..]));

    assert_eq!(items[4].value.as_f64(), Some(42.0));
    assert!(items[4].is_ok());

    // Counters and diagnostics describe the whole batch, exactly as a
    // sequential batch would: five executions, last value from item four.
    assert_eq!(engine.stats().execs, 5);
    assert_eq!(engine.last_value().as_f64(), Some(42.0));
    assert!(engine.last_error().is_none());
}

#[test]
fn one_parallel_worker_matches_exec_many() {
    let sources = ["1 + 1", "1 / 0", "2 * 2", "len(\"abcd\")"];
    let mut sequential = Engine::new();
    let mut parallel = Engine::new();
    let expected = sequential.exec_many(&sources);
    let actual = parallel.exec_parallel(&sources, 1);
    assert_eq!(actual, expected);
    assert_eq!(parallel.stats().execs, sequential.stats().execs);

    // The empty batch expires nothing, like exec_many.
    assert!(parallel.exec_parallel(&[], 8).is_empty());
}

#[test]
fn parallel_workers_inherit_globals_and_run_callbacks_concurrently() {
    let mut engine = Engine::new();
    engine.define("base", Value::from_f64(10.0));
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    engine
        .register_host_fn("bump", 1, move |_engine, args| {
            counter.fetch_add(1, Ordering::Relaxed);
            HostOutcome::Number(args[0].as_f64().unwrap_or(0.0) + 1.0)
        })
        .unwrap();

    let sources: Vec<String> = (0..32)
        .map(|index| format!("bump({index} + base)"))
        .collect();
    let refs: Vec<&str> = sources.iter().map(String::as_str).collect();
    let items = engine.exec_parallel(&refs, 8);

    assert_eq!(calls.load(Ordering::Relaxed), 32);
    for (index, item) in items.iter().enumerate() {
        assert_eq!(item.value.as_f64(), Some(index as f64 + 11.0));
    }
}

#[test]
fn parallel_workers_import_modules_through_their_own_engine() {
    let root = std::env::temp_dir().join(format!(
        "omni-parallel-modules-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("math.omni"), "let answer = 40 + 2").unwrap();

    let mut engine = Engine::new();
    engine.set_module_root(&root).unwrap();
    let items = engine.exec_parallel(
        &[
            "import \"math\"\nmath.answer",
            "import \"math\"\nmath.answer + 1",
        ],
        4,
    );
    assert_eq!(items[0].value.as_f64(), Some(42.0));
    assert_eq!(items[1].value.as_f64(), Some(43.0));

    std::fs::remove_dir_all(&root).unwrap();
}
