//! Multi-node integration tests: real TCP workers on loopback, concurrent
//! clients, and injected faults (dead nodes, wrong PSKs, lying workers).

use omni_cluster::wire::{Message, TaskStatus};
use omni_cluster::{ClusterClient, ErrorCode, RemoteError, WorkerConfig, WorkerNode};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::time::Duration;

fn psk() -> [u8; 32] {
    let mut k = [0u8; 32];
    for (i, b) in k.iter_mut().enumerate() {
        *b = (i as u8).wrapping_mul(7).wrapping_add(3);
    }
    k
}

fn node(gas_cap: u64) -> WorkerNode {
    WorkerNode::start(WorkerConfig {
        psk: psk(),
        gas_cap,
        ..Default::default()
    })
    .expect("worker binds")
}

#[test]
fn free_tier_defaults_bound_cpu_and_memory() {
    let defaults = WorkerConfig::default();
    assert_eq!(defaults.cpu_ms_cap, 50);
    assert_eq!(defaults.memory_mib_cap, 128);
    let worker = node(0);
    let client = ClusterClient::new(vec![worker.addr().to_string()], psk());
    let reply = client.run_remote("40 + 2", 1_000, 0).unwrap();
    assert_eq!(reply.num, 42.0);
    assert_eq!(worker.task_count(), 1);
}

#[test]
fn worker_rejects_tls_config_with_one_side_missing() {
    let result = WorkerNode::start(WorkerConfig {
        tls_cert_pem: Some(std::path::PathBuf::from("missing-cert.pem")),
        tls_key_pem: None,
        ..Default::default()
    });
    assert!(result.is_err());
}

#[test]
fn loopback_executes_and_reports() {
    let worker = node(0);
    let client = ClusterClient::new(vec![worker.addr().to_string()], psk());
    let reply = client.run_remote("40 + 2", 0, 0).unwrap();
    assert_eq!(reply.status, TaskStatus::Ok);
    assert_eq!(reply.num, 42.0);
    assert_eq!(reply.text, "42");
    assert_eq!(worker.task_count(), 1);
}

#[test]
fn compile_and_runtime_errors_cross_identically() {
    let worker = node(0);
    let client = ClusterClient::new(vec![worker.addr().to_string()], psk());
    let bad = client.run_remote("1 +", 0, 0).unwrap();
    assert_eq!(bad.status, TaskStatus::Compile);
    assert!(bad.text.starts_with("[Line 1, Col "), "{}", bad.text);
    let div = client.run_remote("1 / 0", 0, 0).unwrap();
    assert_eq!(div.status, TaskStatus::Runtime);
    assert!(div.text.contains("division"), "{}", div.text);
}

#[test]
fn concurrent_tasks_lose_nothing() {
    let a = node(0);
    let b = node(0);
    let client = Arc::new(ClusterClient::new(
        vec![a.addr().to_string(), b.addr().to_string()],
        psk(),
    ));
    let mut handles = Vec::new();
    for i in 0..64u64 {
        let client = Arc::clone(&client);
        handles.push(std::thread::spawn(move || {
            let reply = client
                .run_remote(&format!("{i} * 2"), 0, 1)
                .expect("task survives concurrency");
            assert_eq!(reply.status, TaskStatus::Ok);
            assert_eq!(reply.num, (i * 2) as f64);
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(a.task_count() + b.task_count(), 64);
}

#[test]
fn gas_cap_is_refused_not_clamped() {
    let worker = node(1_000);
    let client = ClusterClient::new(vec![worker.addr().to_string()], psk());
    let err = client.run_remote("40 + 2", 50_000, 0).unwrap_err();
    assert!(matches!(err, RemoteError::Refused(ErrorCode::GasOverCap)));
    // Under the cap still runs; gas 0 falls back to the cap itself.
    assert_eq!(client.run_remote("40 + 2", 1_000, 0).unwrap().num, 42.0);
    assert_eq!(client.run_remote("40 + 2", 0, 0).unwrap().num, 42.0);
}

#[test]
fn gas_budget_stops_a_runaway_task() {
    let worker = node(0);
    let client = ClusterClient::new(vec![worker.addr().to_string()], psk());
    // A loop that would spin far past any sane budget; the tiny gas limit
    // must surface as a runtime error, not a hung worker.
    let reply = client
        .run_remote("let x = 0\nwhile x < 1000000000 { x = x + 1 }\nx", 500, 0)
        .unwrap();
    assert_eq!(reply.status, TaskStatus::Runtime);
    assert!(!reply.text.is_empty());
    // The worker is still alive afterwards.
    assert_eq!(client.run_remote("7", 0, 0).unwrap().num, 7.0);
}

#[test]
fn remote_tasks_cannot_read_the_filesystem() {
    let worker = node(0);
    let client = ClusterClient::new(vec![worker.addr().to_string()], psk());
    let reply = client
        .run_remote("read_file(\"Cargo.toml\")", 0, 0)
        .unwrap();
    assert_eq!(reply.status, TaskStatus::Runtime);
}

#[test]
fn remote_tasks_cannot_import_modules() {
    let worker = node(0);
    let client = ClusterClient::new(vec![worker.addr().to_string()], psk());
    let reply = client.run_remote("import \"std/math\"", 0, 0).unwrap();
    assert_eq!(reply.status, TaskStatus::Runtime);
}

#[test]
fn wrong_psk_is_refused_and_not_retried() {
    let worker = node(0);
    let client = ClusterClient::new(vec![worker.addr().to_string()], [9u8; 32]);
    let err = client.run_remote("40 + 2", 0, 3).unwrap_err();
    assert!(matches!(err, RemoteError::Tampered), "{err:?}");
    assert_eq!(worker.task_count(), 0);
}

#[test]
fn dead_node_fails_over_with_zero_loss() {
    let mut dead = node(0);
    let dead_addr = dead.addr().to_string();
    dead.stop(); // listener closed: connects are now refused
    drop(dead);
    let alive = node(0);
    let client = ClusterClient::new(vec![dead_addr, alive.addr().to_string()], psk());
    for i in 0..16u64 {
        let reply = client
            .run_remote(&format!("{i} + 100"), 0, 1)
            .expect("failover keeps the task");
        assert_eq!(reply.num, (i + 100) as f64);
    }
    assert_eq!(alive.task_count(), 16);
    assert_eq!(client.healthy_count(), 1);
}

#[test]
fn health_check_marks_and_revives() {
    let a = node(0);
    let b = node(0);
    let client = ClusterClient::new(vec![a.addr().to_string(), b.addr().to_string()], psk());
    assert_eq!(client.health_check(), 2);
    let mut b = b;
    b.stop();
    drop(b);
    assert_eq!(client.health_check(), 1);
    assert_eq!(client.healthy_count(), 1);
    let c = node(0);
    // A fresh pool member at the same address revives on the next check.
    let client2 = ClusterClient::new(vec![a.addr().to_string(), c.addr().to_string()], psk());
    assert_eq!(client2.health_check(), 2);
}

#[test]
fn node_stop_mid_flight_then_failover() {
    let victim = node(0);
    let survivor = node(0);
    let client = ClusterClient::new(
        vec![victim.addr().to_string(), survivor.addr().to_string()],
        psk(),
    )
    .with_timeout(Duration::from_secs(2));
    assert_eq!(client.run_remote("1", 0, 0).unwrap().num, 1.0);
    // Kill the node the rotation just used, then keep dispatching: every
    // task must still come back, from the survivor.
    let mut victim = victim;
    victim.stop();
    drop(victim);
    for i in 0..32u64 {
        let reply = client.run_remote(&format!("{i} * 3"), 0, 2).unwrap();
        assert_eq!(reply.num, (i * 3) as f64);
    }
    assert!(survivor.task_count() >= 32);
}

#[test]
fn quorum_accepts_agreement() {
    let a = node(0);
    let b = node(0);
    let c = node(0);
    let client = ClusterClient::new(
        vec![a.addr().to_string(), b.addr().to_string(), c.addr().to_string()],
        psk(),
    );
    let reply = client.run_remote_verified("6 * 7", 0, 2).unwrap();
    assert_eq!(reply.num, 42.0);
    assert_eq!(reply.text, "42");
}

/// A worker that completes the handshake honestly but lies about the
/// result — the fault quorum verification exists to catch.
fn lying_worker(psk: [u8; 32]) -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let handle = std::thread::spawn(move || {
        for stream in listener.incoming().take(1) {
            let mut stream = stream.unwrap();
            let (mut read, mut write) =
                omni_cluster::wire::handshake(&mut stream, &psk, true).unwrap();
            if let Ok(Message::Task { id, .. }) = read.recv(&mut stream) {
                let lie = Message::Result {
                    id,
                    status: TaskStatus::Ok,
                    num: 41.0,
                    text: b"41".to_vec(),
                };
                let _ = write.send(&mut stream, &lie);
            }
        }
    });
    (addr, handle)
}

#[test]
fn quorum_rejects_a_lying_worker() {
    let honest = node(0);
    let (liar_addr, liar) = lying_worker(psk());
    let client = ClusterClient::new(vec![honest.addr().to_string(), liar_addr], psk());
    let err = client.run_remote_verified("6 * 7", 0, 2).unwrap_err();
    assert!(matches!(err, RemoteError::Verification), "{err:?}");
    drop(client);
    let _ = liar.join();
}

#[test]
fn oversized_source_is_refused_before_execution() {
    let worker = node(0);
    let client = ClusterClient::new(vec![worker.addr().to_string()], psk());
    let big = "1 + ".repeat(40_000); // > 64 KiB
    let err = client.run_remote(&big, 0, 0).unwrap_err();
    assert!(matches!(err, RemoteError::Refused(ErrorCode::TooLong)), "{err:?}");
    assert_eq!(worker.task_count(), 0);
}

#[test]
fn garbage_on_the_wire_does_not_kill_the_worker() {
    let worker = node(0);
    // A plaintext HTTP-ish probe: the handshake read fails, the connection
    // drops, and the worker keeps serving.
    let mut sock = std::net::TcpStream::connect(worker.addr()).unwrap();
    sock.write_all(b"GET / HTTP/1.1\r\n\r\n").unwrap();
    sock.flush().unwrap();
    let mut sink = [0u8; 16];
    let _ = sock.read(&mut sink);
    drop(sock);
    let client = ClusterClient::new(vec![worker.addr().to_string()], psk());
    assert_eq!(client.run_remote("40 + 2", 0, 0).unwrap().num, 42.0);
}
