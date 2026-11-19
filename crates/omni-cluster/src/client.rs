//! The cluster client: dispatch, health, retries, failover, verification.
//!
//! [`ClusterClient`] holds a pool of worker addresses and one PSK. A
//! `run_remote` walks the pool in rotation: a transport failure marks the
//! node unhealthy and the task moves to the next healthy node, so a dead
//! worker costs one refused connection, not a lost task. `run_remote_verified`
//! goes further — it submits to `quorum` *distinct* nodes and requires
//! identical results, so a node that answers wrongly (rather than not at
//! all) is caught too.

use crate::wire::{self, ErrorCode, Message, TaskStatus, ValueTag};
use std::fmt;
use std::io;
use std::io::Write;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use rustls::RootCertStore;
use rustls::pki_types::ServerName;

/// Default per-attempt socket timeout: connect, handshake, send, receive.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// What a worker answered for a task that ran (as opposed to a transport or
/// protocol failure, which is a [`RemoteError`]).
///
/// `kind` is the value's own tag: the client uses it to rebuild the exact value
/// the worker produced, so `42`, `"42"`, `true`, and `nil` stay distinguishable
/// across the network the same way they stay distinguishable across the C ABI.
/// For a task that failed, `kind` is [`ValueTag::Nil`] and `status` names the
/// failure.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskReply {
    pub status: TaskStatus,
    pub kind: ValueTag,
    pub num: f64,
    pub text: String,
}

/// Every way a remote execution can fail short of a task-level answer.
#[derive(Debug)]
pub enum RemoteError {
    /// DNS/connect/socket failure. The node is marked unhealthy.
    Io(String),
    /// The address string did not parse.
    BadAddr(String),
    /// The record failed authentication: wrong PSK, tampering, or replay.
    /// Not retried against the same node — it cannot fix itself.
    Tampered,
    /// The connection closed mid-exchange.
    Disconnected,
    /// The peer spoke something the protocol does not allow.
    Malformed,
    /// The worker refused the task (gas over cap, too long, busy, ...).
    Refused(ErrorCode),
    /// Every node and every retry was exhausted without an answer.
    ExhaustedRetries,
    /// Fewer than `quorum` distinct nodes agreed on the result.
    Verification,
    /// The pool is empty or every node is marked unhealthy.
    NoHealthyNode,
}

impl fmt::Display for RemoteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RemoteError::Io(e) => write!(f, "cluster: transport failure: {e}"),
            RemoteError::BadAddr(a) => write!(f, "cluster: bad worker address: {a}"),
            RemoteError::Tampered => write!(f, "cluster: record authentication failed"),
            RemoteError::Disconnected => write!(f, "cluster: worker disconnected mid-exchange"),
            RemoteError::Malformed => write!(f, "cluster: malformed protocol message"),
            RemoteError::Refused(code) => write!(f, "cluster: worker refused task ({code:?})"),
            RemoteError::ExhaustedRetries => write!(f, "cluster: all workers and retries exhausted"),
            RemoteError::Verification => write!(f, "cluster: workers disagreed on the result"),
            RemoteError::NoHealthyNode => write!(f, "cluster: no healthy worker in the pool"),
        }
    }
}

impl std::error::Error for RemoteError {}

fn classify_io(e: io::Error) -> RemoteError {
    match e.kind() {
        io::ErrorKind::InvalidData => {
            if e.to_string().contains("auth") {
                RemoteError::Tampered
            } else {
                RemoteError::Malformed
            }
        }
        io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset | io::ErrorKind::BrokenPipe => {
            RemoteError::Disconnected
        }
        _ => RemoteError::Io(e.to_string()),
    }
}

/// One request over one fresh connection: handshake, send, receive.
pub fn submit_once(
    addr: &str,
    psk: &[u8; 32],
    request: &Message,
    timeout: Duration,
) -> Result<Message, RemoteError> {
    submit_once_inner(addr, psk, request, timeout, None)
}

fn submit_once_inner(
    addr: &str,
    psk: &[u8; 32],
    request: &Message,
    timeout: Duration,
    tls: Option<(&[u8], &str)>,
) -> Result<Message, RemoteError> {
    let sock_addr: SocketAddr = addr
        .to_socket_addrs()
        .map_err(|_| RemoteError::BadAddr(addr.to_string()))?
        .next()
        .ok_or_else(|| RemoteError::BadAddr(addr.to_string()))?;
    let mut stream = TcpStream::connect_timeout(&sock_addr, timeout)
        .map_err(|e| RemoteError::Io(e.to_string()))?;
    stream.set_read_timeout(Some(timeout)).map_err(|e| RemoteError::Io(e.to_string()))?;
    stream.set_write_timeout(Some(timeout)).map_err(|e| RemoteError::Io(e.to_string()))?;
    if let Some((ca_pem, server_name)) = tls {
        let mut roots = RootCertStore::empty();
        let mut reader = io::BufReader::new(ca_pem);
        let certs = rustls_pemfile::certs(&mut reader)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| RemoteError::Io(e.to_string()))?;
        let (valid, _) = roots.add_parsable_certificates(certs);
        if valid == 0 {
            return Err(RemoteError::Io("TLS CA bundle contained no valid certificates".to_string()));
        }
        let config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let name = ServerName::try_from(server_name.to_string())
            .map_err(|e| RemoteError::BadAddr(e.to_string()))?;
        let connection = rustls::ClientConnection::new(std::sync::Arc::new(config), name)
            .map_err(|e| RemoteError::Io(e.to_string()))?;
        let mut tls_stream = rustls::StreamOwned::new(connection, stream);
        tls_stream
            .flush()
            .map_err(|e| RemoteError::Io(e.to_string()))?;
        let (mut read, mut write) =
            wire::handshake(&mut tls_stream, psk, false).map_err(classify_io)?;
        write.send(&mut tls_stream, request).map_err(classify_io)?;
        let reply = read.recv(&mut tls_stream).map_err(classify_io)?;
        return match reply {
            Message::Error { code, .. } => Err(RemoteError::Refused(code)),
            other => Ok(other),
        };
    }
    let (mut read, mut write) =
        wire::handshake(&mut stream, psk, false).map_err(classify_io)?;
    write.send(&mut stream, request).map_err(classify_io)?;
    let reply = read.recv(&mut stream).map_err(classify_io)?;
    match reply {
        Message::Error { code, .. } => Err(RemoteError::Refused(code)),
        other => Ok(other),
    }
}

/// Turn a worker's reply into a [`TaskReply`], or `None` when it is not an
/// answer to `id` (a mismatched id is a lying or confused peer) or carries no
/// valid UTF-8.
///
/// Both the tagged [`Message::Value`] and the older rendered
/// [`Message::Result`] are accepted, so a worker built before the value tag
/// still answers — its reply is carried as a string-kind result rather than
/// failing the task.
fn reply_for(id: u64, reply: Message) -> Option<TaskReply> {
    match reply {
        Message::Value {
            id: reply_id,
            tag,
            num,
            text,
        } if reply_id == id => String::from_utf8(text).ok().map(|text| TaskReply {
            status: TaskStatus::Ok,
            kind: tag,
            num,
            text,
        }),
        Message::Result {
            id: reply_id,
            status,
            num,
            text,
        } if reply_id == id => String::from_utf8(text).ok().map(|text| TaskReply {
            status,
            kind: ValueTag::String,
            num,
            text,
        }),
        _ => None,
    }
}

struct PoolNode {
    addr: String,
    healthy: AtomicBool,
}
/// A pool of workers sharing one PSK, with rotation, health marking, retries,
/// failover, and quorum verification.
pub struct ClusterClient {
    nodes: Vec<PoolNode>,
    psk: [u8; 32],
    timeout: Duration,
    next: AtomicU64,
    tls_roots_pem: Option<Vec<u8>>,
    tls_server_name: Option<String>,
}

impl ClusterClient {
    pub fn new(addrs: Vec<String>, psk: [u8; 32]) -> ClusterClient {
        ClusterClient {
            nodes: addrs
                .into_iter()
                .map(|addr| PoolNode {
                    addr,
                    healthy: AtomicBool::new(true),
                })
                .collect(),
            psk,
            timeout: DEFAULT_TIMEOUT,
            next: AtomicU64::new(0),
            tls_roots_pem: None,
            tls_server_name: None,
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Configure certificate-verified TLS in front of the PSK channel.
    /// `ca_pem` contains PEM CA certificates; `server_name` is verified by
    /// rustls and must match the worker certificate SAN.
    pub fn with_tls(mut self, ca_pem: &[u8], server_name: &str) -> Self {
        self.tls_roots_pem = Some(ca_pem.to_vec());
        self.tls_server_name = Some(server_name.to_string());
        self
    }

    fn submit(&self, addr: &str, request: &Message) -> Result<Message, RemoteError> {
        match (&self.tls_roots_pem, &self.tls_server_name) {
            (Some(ca), Some(name)) => submit_once_inner(addr, &self.psk, request, self.timeout, Some((ca, name))),
            _ => submit_once(addr, &self.psk, request, self.timeout),
        }
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Ping every node and record which answered. Returns the healthy count.
    /// A node marked unhealthy here is skipped by `run_remote` until a later
    /// health check revives it.
    pub fn health_check(&self) -> usize {
        let mut healthy = 0;
        for node in &self.nodes {
            let up = matches!(
                self.submit(&node.addr, &Message::Ping),
                Ok(Message::Pong)
            );
            node.healthy.store(up, Ordering::SeqCst);
            if up {
                healthy += 1;
            }
        }
        healthy
    }

    pub fn healthy_count(&self) -> usize {
        self.nodes.iter().filter(|n| n.healthy.load(Ordering::SeqCst)).count()
    }

    /// Execute `source` on a healthy worker, failing over across the pool.
    /// `retries` is the number of extra full passes over the healthy nodes
    /// after the first; 0 still tries every healthy node once.
    pub fn run_remote(
        &self,
        source: &str,
        gas: u64,
        retries: u32,
    ) -> Result<TaskReply, RemoteError> {
        if self.nodes.is_empty() {
            return Err(RemoteError::NoHealthyNode);
        }
        let mut last_err = RemoteError::NoHealthyNode;
        for _ in 0..=retries {
            let start = self.next.fetch_add(1, Ordering::SeqCst) as usize;
            let mut tried_any = false;
            for offset in 0..self.nodes.len() {
                let node = &self.nodes[(start + offset) % self.nodes.len()];
                if !node.healthy.load(Ordering::SeqCst) {
                    continue;
                }
                tried_any = true;
                let id = self.next.fetch_add(1, Ordering::SeqCst);
                let request = Message::Task {
                    id,
                    gas,
                    source: source.as_bytes().to_vec(),
                };
                match self.submit(&node.addr, &request) {
                    Ok(reply) => match reply_for(id, reply) {
                        Some(task) => return Ok(task),
                        None => {
                            node.healthy.store(false, Ordering::SeqCst);
                            last_err = RemoteError::Malformed;
                        }
                    },
                    Err(RemoteError::Refused(code)) => {
                        // A refusal is the worker speaking, not the network
                        // failing: the node stays healthy, and the caller
                        // gets the refusal rather than a silent retry.
                        return Err(RemoteError::Refused(code));
                    }
                    Err(RemoteError::Tampered) => {
                        node.healthy.store(false, Ordering::SeqCst);
                        return Err(RemoteError::Tampered);
                    }
                    Err(RemoteError::Io(_) | RemoteError::Disconnected) => {
                        // Transient socket loss gets bounded same-node retries.
                        // It does not consume the caller's full-pass retry
                        // budget and it does not declare the node unhealthy
                        // merely because one loopback accept raced.
                        let mut recovered = None;
                        let mut retry_err = None;
                        for _ in 0..3 {
                            match self.submit(&node.addr, &request) {
                                Ok(reply) => {
                                    if let Some(task) = reply_for(id, reply) {
                                        recovered = Some(task);
                                        break;
                                    }
                                    retry_err = Some(RemoteError::Malformed);
                                    break;
                                }
                                Err(RemoteError::Refused(code)) => {
                                    return Err(RemoteError::Refused(code));
                                }
                                Err(RemoteError::Tampered) => {
                                    node.healthy.store(false, Ordering::SeqCst);
                                    return Err(RemoteError::Tampered);
                                }
                                Err(e) => retry_err = Some(e),
                            }
                        }
                        if let Some(task) = recovered {
                            return Ok(task);
                        }
                        last_err = retry_err.unwrap_or(last_err);
                    }
                    Err(e) => {
                        node.healthy.store(false, Ordering::SeqCst);
                        last_err = e;
                    }
                }
            }
            if !tried_any {
                return Err(RemoteError::NoHealthyNode);
            }
        }
        Err(match last_err {
            RemoteError::NoHealthyNode => RemoteError::ExhaustedRetries,
            other => other,
        })
    }

    /// Execute on `quorum` distinct healthy workers and require identical
    /// results (status, numeric bits, and rendered text). A worker that
    /// disagrees is marked unhealthy; agreement short of the quorum is a
    /// [`RemoteError::Verification`], never a silently accepted answer.
    pub fn run_remote_verified(
        &self,
        source: &str,
        gas: u64,
        quorum: usize,
    ) -> Result<TaskReply, RemoteError> {
        if quorum == 0 || quorum > self.nodes.len() {
            return Err(RemoteError::Verification);
        }
        let start = self.next.fetch_add(1, Ordering::SeqCst) as usize;
        let mut replies: Vec<(usize, TaskReply)> = Vec::new();
        for offset in 0..self.nodes.len() {
            let idx = (start + offset) % self.nodes.len();
            let node = &self.nodes[idx];
            if !node.healthy.load(Ordering::SeqCst) {
                continue;
            }
            let id = self.next.fetch_add(1, Ordering::SeqCst);
            let request = Message::Task {
                id,
                gas,
                source: source.as_bytes().to_vec(),
            };
            match self.submit(&node.addr, &request) {
                Ok(reply) => match reply_for(id, reply) {
                    Some(task) => replies.push((idx, task)),
                    None => node.healthy.store(false, Ordering::SeqCst),
                },
                Err(RemoteError::Refused(code)) => return Err(RemoteError::Refused(code)),
                Err(_) => node.healthy.store(false, Ordering::SeqCst),
            }
            if replies.len() >= quorum {
                break;
            }
        }
        if replies.len() < quorum {
            return Err(RemoteError::Verification);
        }
        let first = &replies[0].1;
        for (idx, reply) in &replies[1..] {
            let same = reply.status == first.status
                && reply.kind == first.kind
                && reply.num.to_bits() == first.num.to_bits()
                && reply.text == first.text;
            if !same {
                self.nodes[*idx].healthy.store(false, Ordering::SeqCst);
                return Err(RemoteError::Verification);
            }
        }
        Ok(first.clone())
    }
}
