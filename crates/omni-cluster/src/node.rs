//! The worker node: a TCP listener that executes sandboxed tasks.
//!
//! Each connection carries one handshake and one request. Tasks run on
//! `Engine::fork` of a template engine built once at startup, so no task can
//! observe another task's bindings, arena, or counters, and no task can widen
//! the node's permission set: filesystem roots come only from
//! [`WorkerConfig`], there is no module root (remote imports are refused by
//! the engine itself), and the gas ceiling is enforced per task via
//! `set_step_limit`.

use crate::wire::{self, ErrorCode, MAX_CLUSTER_TASK, Message, TaskStatus, ValueTag};
use omni_core::{
    CPU_LIMIT_MESSAGE, DEFAULT_STEP_LIMIT, Engine, ErrorKind, MEMORY_LIMIT_MESSAGE, Value,
};
use rustls::ServerConfig;
use rustls_pemfile::{certs, private_key};
use std::io;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Everything a node operator chooses before the node accepts traffic.
/// Nothing here is reachable from the wire: a task cannot grant itself a
/// filesystem root, raise the gas cap, or register a host function.
#[derive(Debug, Clone)]
pub struct WorkerConfig {
    /// Address to bind. `"127.0.0.1:0"` picks an ephemeral port, which
    /// [`WorkerNode::addr`] then reports.
    pub bind_addr: String,
    /// The pre-shared key both sides must hold. Generate 32 random bytes;
    /// this is the node's only authentication boundary.
    pub psk: [u8; 32],
    /// Maximum gas any single task may request. Zero means tasks are limited
    /// only by [`DEFAULT_STEP_LIMIT`] (and a task asking for 0 gets that
    /// default). A non-zero cap is refused *above*, not clamped: the client
    /// learns its request was rejected rather than silently throttled.
    pub gas_cap: u64,
    /// Filesystem roots remote tasks may read. Empty means no reads.
    pub fs_read_roots: Vec<PathBuf>,
    /// Maximum wall-clock CPU budget per task, in milliseconds.
    /// Free-tier policy default is 50 ms.
    pub cpu_ms_cap: u64,
    /// Maximum sandbox arena/egress memory per task, in MiB.
    /// Free-tier policy default is 128 MiB.
    pub memory_mib_cap: u64,
    /// Optional TLS server certificate PEM. When set, `tls_key_pem` must
    /// also be set and new connections require TLS.
    pub tls_cert_pem: Option<PathBuf>,
    /// Optional TLS server private-key PEM.
    pub tls_key_pem: Option<PathBuf>,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        WorkerConfig {
            bind_addr: "127.0.0.1:0".to_string(),
            psk: [0u8; 32],
            gas_cap: 0,
            fs_read_roots: Vec::new(),
            cpu_ms_cap: 50,
            memory_mib_cap: 128,
            tls_cert_pem: None,
            tls_key_pem: None,
        }
    }
}

/// A running worker. Dropping it (or calling [`WorkerNode::stop`]) stops the
/// accept loop and closes the listener, so new connections are refused
/// immediately; in-flight tasks finish under their own gas budget.
pub struct WorkerNode {
    addr: SocketAddr,
    tasks: Arc<AtomicU64>,
    stopping: Arc<AtomicBool>,
    accept: Option<JoinHandle<()>>,
}

impl WorkerNode {
    pub fn start(config: WorkerConfig) -> io::Result<WorkerNode> {
        let tls = match (&config.tls_cert_pem, &config.tls_key_pem) {
            (None, None) => None,
            (Some(_), None) | (None, Some(_)) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "TLS requires both tls_cert_pem and tls_key_pem",
                ));
            }
            (Some(cert_path), Some(key_path)) => {
                Some(Arc::new(load_tls_config(cert_path, key_path)?))
            }
        };
        let mut template = Engine::new();
        for root in &config.fs_read_roots {
            template.allow_fs_read(root)?;
        }
        let template = Arc::new(Mutex::new(template));
        let listener = TcpListener::bind(&config.bind_addr)?;
        let addr = listener.local_addr()?;
        listener.set_nonblocking(true)?;
        let tasks = Arc::new(AtomicU64::new(0));
        let stopping = Arc::new(AtomicBool::new(false));
        let psk = config.psk;
        let gas_cap = config.gas_cap;
        let cpu_ms_cap = config.cpu_ms_cap;
        let memory_mib_cap = config.memory_mib_cap;
        let accept_tasks = Arc::clone(&tasks);
        let accept_stop = Arc::clone(&stopping);
        let accept = std::thread::spawn(move || {
            while !accept_stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let template = Arc::clone(&template);
                        let tasks = Arc::clone(&accept_tasks);
                        let tls = tls.clone();
                        // Detached: a task is bounded by gas, and stop()
                        // does not wait for it — the listener is already
                        // closed by then, which is the fault signal clients
                        // act on.
                        let _ = std::thread::spawn(move || {
                            let _ = serve(
                                stream,
                                &psk,
                                gas_cap,
                                cpu_ms_cap,
                                memory_mib_cap,
                                &template,
                                &tasks,
                                tls.clone(),
                            );
                        });
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        });
        Ok(WorkerNode {
            addr,
            tasks,
            stopping,
            accept: Some(accept),
        })
    }

    /// The address actually bound (resolves an ephemeral port).
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// How many tasks this node has executed (compile/runtime failures
    /// included — the engine ran, it just answered with an error).
    pub fn task_count(&self) -> u64 {
        self.tasks.load(Ordering::SeqCst)
    }

    pub fn stop(&mut self) {
        self.stopping.store(true, Ordering::SeqCst);
        if let Some(handle) = self.accept.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for WorkerNode {
    fn drop(&mut self) {
        self.stop();
    }
}

fn load_tls_config(
    cert_path: &std::path::Path,
    key_path: &std::path::Path,
) -> io::Result<ServerConfig> {
    let mut cert_reader = io::BufReader::new(std::fs::File::open(cert_path)?);
    let certificates = certs(&mut cert_reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if certificates.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "TLS certificate PEM contains no certificates",
        ));
    }
    let mut key_reader = io::BufReader::new(std::fs::File::open(key_path)?);
    let key = private_key(&mut key_reader)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "TLS private-key PEM contains no supported key",
            )
        })?;
    ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certificates, key)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn serve(
    mut stream: TcpStream,
    psk: &[u8; 32],
    gas_cap: u64,
    cpu_ms_cap: u64,
    memory_mib_cap: u64,
    template: &Mutex<Engine>,
    tasks: &AtomicU64,
    tls: Option<Arc<ServerConfig>>,
) -> io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    if let Some(config) = tls {
        let connection = rustls::ServerConnection::new(config)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let mut tls_stream = rustls::StreamOwned::new(connection, stream);
        return serve_inner(
            &mut tls_stream,
            psk,
            gas_cap,
            cpu_ms_cap,
            memory_mib_cap,
            template,
            tasks,
        );
    }
    serve_inner(
        &mut stream,
        psk,
        gas_cap,
        cpu_ms_cap,
        memory_mib_cap,
        template,
        tasks,
    )
}

fn serve_inner(
    stream: &mut (impl io::Read + io::Write),
    psk: &[u8; 32],
    gas_cap: u64,
    cpu_ms_cap: u64,
    memory_mib_cap: u64,
    template: &Mutex<Engine>,
    tasks: &AtomicU64,
) -> io::Result<()> {
    let mut stream = stream;
    let (mut read, mut write) = wire::handshake(&mut stream, psk, true)?;
    let request = read.recv(&mut stream)?;
    let reply = match request {
        Message::Ping => Message::Pong,
        Message::Task { id, gas, source } => execute_task(
            id,
            gas,
            &source,
            gas_cap,
            cpu_ms_cap,
            memory_mib_cap,
            None,
            template,
            tasks,
        ),
        Message::TaskWithLimits {
            id,
            gas,
            cpu_ms,
            memory_mib,
            source,
        } => execute_task(
            id,
            gas,
            &source,
            gas_cap,
            cpu_ms_cap,
            memory_mib_cap,
            Some((cpu_ms, memory_mib)),
            template,
            tasks,
        ),
        _ => Message::Error {
            id: 0,
            code: ErrorCode::Malformed,
        },
    };
    write.send(&mut stream, &reply)
}

fn execute_task(
    id: u64,
    gas: u64,
    source: &[u8],
    gas_cap: u64,
    configured_cpu_ms_cap: u64,
    configured_memory_mib_cap: u64,
    request_limits: Option<(u64, u64)>,
    template: &Mutex<Engine>,
    tasks: &AtomicU64,
) -> Message {
    let (requested_cpu_ms, requested_memory_mib) = request_limits.unwrap_or((0, 0));
    let cpu_ms_cap = stricter_limit(configured_cpu_ms_cap, requested_cpu_ms);
    let memory_mib_cap = stricter_limit(configured_memory_mib_cap, requested_memory_mib);
    if source.len() > MAX_CLUSTER_TASK {
        return Message::Error {
            id,
            code: ErrorCode::TooLong,
        };
    }
    if gas_cap > 0 && gas > gas_cap {
        return Message::Error {
            id,
            code: ErrorCode::GasOverCap,
        };
    }
    let Ok(source) = std::str::from_utf8(source) else {
        return Message::Error {
            id,
            code: ErrorCode::Malformed,
        };
    };
    let effective_gas = if gas == 0 {
        if gas_cap > 0 {
            gas_cap
        } else {
            DEFAULT_STEP_LIMIT
        }
    } else {
        gas
    };
    // The lock is held only for the fork; execution itself is lock-free, so
    // concurrent tasks share nothing but the immutable template snapshot.
    let mut engine = match template.lock() {
        Ok(t) => t.fork(),
        Err(_) => {
            return Message::Error {
                id,
                code: ErrorCode::Internal,
            };
        }
    };
    engine.set_step_limit(effective_gas);
    if cpu_ms_cap > 0 {
        engine.set_time_limit(Some(Duration::from_millis(cpu_ms_cap)));
    }
    if memory_mib_cap > 0
        && !engine.set_memory_limit(
            memory_mib_cap
                .saturating_mul(1024 * 1024)
                .try_into()
                .unwrap_or(usize::MAX),
        )
    {
        return Message::Error {
            id,
            code: ErrorCode::MemoryLimit,
        };
    }
    tasks.fetch_add(1, Ordering::SeqCst);
    let started = Instant::now();
    let execution = engine.exec(source);
    if cpu_ms_cap > 0 && started.elapsed() >= Duration::from_millis(cpu_ms_cap) {
        return Message::Error {
            id,
            code: ErrorCode::CpuLimit,
        };
    }
    let stats = engine.stats();
    let bytes_reserved = stats
        .arena_bytes_reserved
        .saturating_add(stats.egress_bytes_reserved);
    if memory_mib_cap > 0 && bytes_reserved > memory_mib_cap.saturating_mul(1024 * 1024) {
        return Message::Error {
            id,
            code: ErrorCode::MemoryLimit,
        };
    }
    match execution {
        Ok(value) => {
            let text = engine.render(value);
            if text.len() > MAX_CLUSTER_TASK {
                return Message::Error {
                    id,
                    code: ErrorCode::TooLong,
                };
            }
            Message::Value {
                id,
                tag: value_tag(value),
                num: value.as_f64().unwrap_or(f64::NAN),
                text: text.into_bytes(),
            }
        }
        Err(e) if e.message == CPU_LIMIT_MESSAGE => Message::Error {
            id,
            code: ErrorCode::CpuLimit,
        },
        Err(e) if e.message == MEMORY_LIMIT_MESSAGE => Message::Error {
            id,
            code: ErrorCode::MemoryLimit,
        },
        Err(e) => {
            let status = match e.kind {
                ErrorKind::Lex | ErrorKind::Parse => TaskStatus::Compile,
                _ => TaskStatus::Runtime,
            };
            let text = e.to_string();
            if text.len() > MAX_CLUSTER_TASK {
                return Message::Error {
                    id,
                    code: ErrorCode::TooLong,
                };
            }
            Message::Result {
                id,
                status,
                num: f64::NAN,
                text: text.into_bytes(),
            }
        }
    }
}

fn stricter_limit(configured: u64, requested: u64) -> u64 {
    match (configured, requested) {
        (0, limit) | (limit, 0) => limit,
        (configured, requested) => configured.min(requested),
    }
}

/// Classify a successful value the way the C ABI's `encode` does, so a remote
/// caller can tell `42` from `"42"` from `true` from `nil`. The order matters:
/// a string is checked first, and a boolean's singleton bits are not a double.
fn value_tag(value: Value) -> ValueTag {
    if value.is_string() {
        ValueTag::String
    } else if value.as_f64().is_some() {
        ValueTag::Number
    } else if value.as_bool().is_some() {
        ValueTag::Bool
    } else if value == Value::UNDEF {
        ValueTag::Undef
    } else {
        ValueTag::Nil
    }
}
