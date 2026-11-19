//! Wire framing and message codec for the cluster protocol.
//!
//! Every connection carries exactly one request and its reply (or a streamed
//! pair for ping/pong): the client connects, performs the nonce handshake,
//! sends one sealed record, reads one sealed record, and closes. Frames are
//! length-prefixed with a 4-byte little-endian length covering the sealed
//! record, capped at [`MAX_FRAME`]; the plaintext inside is capped at
//! [`MAX_CLUSTER_TASK`] bytes so a node can bound per-task memory before it
//! has parsed anything.

use crate::crypto;
use std::io::{self, Read, Write};

/// Maximum plaintext source a remote task may carry (64 KiB).
pub const MAX_CLUSTER_TASK: usize = 64 * 1024;

/// Maximum sealed frame on the wire: task cap plus AEAD tag, nonce trailer,
/// and encoding slack. Anything longer is refused before a single byte beyond
/// the length prefix is read.
pub const MAX_FRAME: usize = MAX_CLUSTER_TASK + 1024;

/// Wire nonce length exchanged in cleartext during the handshake. Both
/// directions mix both nonces into the channel keys, so a passive observer
/// who recorded a previous session cannot replay it under the same PSK.
pub const WIRE_NONCE_LEN: usize = 12;

/// How many concurrent tasks a worker reports before it starts refusing new
/// ones with `ErrorCode::Busy`. Zero means unbounded (the node's own
/// capacity is the limit).
pub const DEFAULT_BACKLOG: usize = 128;

/// Status of an executed task, mirroring the FFI status codes so a client
/// can forward them unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    Ok,
    Compile,
    Runtime,
}

/// The kind of value a successful task produced.
///
/// The tag crosses the wire so a caller can tell `42` from `"42"` and from
/// `nil`. Without it the client could only forward rendered text, which is
/// indistinguishable from a string that happens to spell the same thing — the
/// same reason the C ABI carries a tag beside the payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueTag {
    Number,
    Bool,
    String,
    Nil,
    Undef,
}

impl ValueTag {
    pub fn to_u32(self) -> u32 {
        match self {
            ValueTag::Number => 0,
            ValueTag::Bool => 1,
            ValueTag::String => 2,
            ValueTag::Nil => 3,
            ValueTag::Undef => 4,
        }
    }

    pub fn from_u32(v: u32) -> Option<Self> {
        match v {
            0 => Some(ValueTag::Number),
            1 => Some(ValueTag::Bool),
            2 => Some(ValueTag::String),
            3 => Some(ValueTag::Nil),
            4 => Some(ValueTag::Undef),
            _ => None,
        }
    }
}

impl TaskStatus {
    pub fn to_u32(self) -> u32 {
        match self {
            TaskStatus::Ok => 0,
            TaskStatus::Compile => 1,
            TaskStatus::Runtime => 2,
        }
    }

    pub fn from_u32(v: u32) -> Option<Self> {
        match v {
            0 => Some(TaskStatus::Ok),
            1 => Some(TaskStatus::Compile),
            2 => Some(TaskStatus::Runtime),
            _ => None,
        }
    }
}

/// Wire-level error codes carried in [`Message::Error`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    /// Frame or payload could not be decoded.
    Malformed = 1,
    /// Payload exceeded a size limit.
    TooLong = 2,
    /// Requested gas exceeds the node's configured cap.
    GasOverCap = 3,
    /// Node is at its task backlog limit.
    Busy = 4,
    /// Internal failure while forking or executing.
    Internal = 5,
    /// Handshake nonce exchange failed.
    Handshake = 6,
    /// CPU wall-clock limit exceeded.
    CpuLimit = 7,
    /// Sandbox memory limit exceeded.
    MemoryLimit = 8,
}

impl ErrorCode {
    pub fn to_u32(self) -> u32 {
        self as u32
    }

    pub fn from_u32(v: u32) -> Option<Self> {
        match v {
            1 => Some(ErrorCode::Malformed),
            2 => Some(ErrorCode::TooLong),
            3 => Some(ErrorCode::GasOverCap),
            4 => Some(ErrorCode::Busy),
            5 => Some(ErrorCode::Internal),
            6 => Some(ErrorCode::Handshake),
            7 => Some(ErrorCode::CpuLimit),
            8 => Some(ErrorCode::MemoryLimit),
            _ => None,
        }
    }
}

/// A protocol message. `Task` and `Ping` flow client→worker; `Result` and
/// `Pong` flow worker→client; `Error` can flow either way.
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    Task {
        id: u64,
        gas: u64,
        source: Vec<u8>,
    },
    Result {
        id: u64,
        status: TaskStatus,
        num: f64,
        text: Vec<u8>,
    },
    /// A successful result carrying its own value kind, so a caller can
    /// reconstruct `42`, `"42"`, `true`, and `nil` as the engine meant them.
    /// `text` is the rendered form and is always present; `num` is meaningful
    /// only for `Number`/`Bool`.
    Value {
        id: u64,
        tag: ValueTag,
        num: f64,
        text: Vec<u8>,
    },
    Ping,
    Pong,
    Error {
        id: u64,
        code: ErrorCode,
    },
}

// Plaintext layouts (all integers little-endian):
//   Task:    u8=0 | u64 id | u64 gas | u32 src_len | src bytes
//   Result:  u8=1 | u64 id | u32 status | f64 num | u32 text_len | text
//   Ping:    u8=2
//   Pong:    u8=3
//   Error:   u8=4 | u64 id | u32 code
//   Value:   u8=5 | u64 id | u32 tag | f64 num | u32 text_len | text

const TAG_TASK: u8 = 0;
const TAG_RESULT: u8 = 1;
const TAG_PING: u8 = 2;
const TAG_PONG: u8 = 3;
const TAG_ERROR: u8 = 4;
const TAG_VALUE: u8 = 5;

impl Message {
    /// Encode to plaintext. The result is at most [`MAX_CLUSTER_TASK`] +
    /// fixed-header bytes; callers cap `source`/`text` before encoding.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Message::Task { id, gas, source } => {
                out.push(TAG_TASK);
                out.extend_from_slice(&id.to_le_bytes());
                out.extend_from_slice(&gas.to_le_bytes());
                out.extend_from_slice(&(source.len() as u32).to_le_bytes());
                out.extend_from_slice(source);
            }
            Message::Result {
                id,
                status,
                num,
                text,
            } => {
                out.push(TAG_RESULT);
                out.extend_from_slice(&id.to_le_bytes());
                out.extend_from_slice(&status.to_u32().to_le_bytes());
                out.extend_from_slice(&num.to_bits().to_le_bytes());
                out.extend_from_slice(&(text.len() as u32).to_le_bytes());
                out.extend_from_slice(text);
            }
            Message::Ping => out.push(TAG_PING),
            Message::Pong => out.push(TAG_PONG),
            Message::Value {
                id,
                tag,
                num,
                text,
            } => {
                out.push(TAG_VALUE);
                out.extend_from_slice(&id.to_le_bytes());
                out.extend_from_slice(&tag.to_u32().to_le_bytes());
                out.extend_from_slice(&num.to_bits().to_le_bytes());
                out.extend_from_slice(&(text.len() as u32).to_le_bytes());
                out.extend_from_slice(text);
            }
            Message::Error { id, code } => {
                out.push(TAG_ERROR);
                out.extend_from_slice(&id.to_le_bytes());
                out.extend_from_slice(&code.to_u32().to_le_bytes());
            }
        }
        out
    }

    /// Decode a plaintext frame. Returns `None` on any malformation; the
    /// caller turns that into `ErrorCode::Malformed`. Trailing bytes are
    /// rejected — a frame carries exactly one message.
    pub fn decode(buf: &[u8]) -> Option<Message> {
        let (&tag, rest) = buf.split_first()?;
        match tag {
            TAG_TASK => {
                if rest.len() < 20 {
                    return None;
                }
                let id = u64::from_le_bytes(rest[0..8].try_into().ok()?);
                let gas = u64::from_le_bytes(rest[8..16].try_into().ok()?);
                let len = u32::from_le_bytes(rest[16..20].try_into().ok()?) as usize;
                let src = rest.get(20..20usize.checked_add(len)?)?;
                if src.len() != len {
                    return None;
                }
                Some(Message::Task {
                    id,
                    gas,
                    source: src.to_vec(),
                })
            }
            TAG_RESULT => {
                if rest.len() < 24 {
                    return None;
                }
                let id = u64::from_le_bytes(rest[0..8].try_into().ok()?);
                let status = TaskStatus::from_u32(u32::from_le_bytes(
                    rest[8..12].try_into().ok()?,
                ))?;
                let num = f64::from_bits(u64::from_le_bytes(rest[12..20].try_into().ok()?));
                let len = u32::from_le_bytes(rest[20..24].try_into().ok()?) as usize;
                let text = rest.get(24..24usize.checked_add(len)?)?;
                if text.len() != len {
                    return None;
                }
                Some(Message::Result {
                    id,
                    status,
                    num,
                    text: text.to_vec(),
                })
            }
            TAG_PING if rest.is_empty() => Some(Message::Ping),
            TAG_PONG if rest.is_empty() => Some(Message::Pong),
            TAG_VALUE => {
                if rest.len() < 24 {
                    return None;
                }
                let id = u64::from_le_bytes(rest[0..8].try_into().ok()?);
                let tag = ValueTag::from_u32(u32::from_le_bytes(rest[8..12].try_into().ok()?))?;
                let num = f64::from_bits(u64::from_le_bytes(rest[12..20].try_into().ok()?));
                let len = u32::from_le_bytes(rest[20..24].try_into().ok()?) as usize;
                let text = rest.get(24..24usize.checked_add(len)?)?;
                if text.len() != len {
                    return None;
                }
                Some(Message::Value {
                    id,
                    tag,
                    num,
                    text: text.to_vec(),
                })
            }
            TAG_ERROR => {
                if rest.len() != 12 {
                    return None;
                }
                let id = u64::from_le_bytes(rest[0..8].try_into().ok()?);
                let code = ErrorCode::from_u32(u32::from_le_bytes(rest[8..12].try_into().ok()?))?;
                Some(Message::Error { id, code })
            }
            _ => None,
        }
    }
}

/// One direction of an encrypted channel: a 32-byte AEAD key, the next
/// sequence number, and the peer nonce mixed in at handshake. Sequence
/// numbers are monotonic per connection and serve as both the AEAD nonce
/// tail and additional authenticated data, so a record replayed, reordered,
/// or spliced from another connection fails authentication.
pub struct Channel {
    key: [u8; 32],
    seq: u64,
    nonce: [u8; WIRE_NONCE_LEN],
}

impl Channel {
    pub fn new(key: [u8; 32], nonce: [u8; WIRE_NONCE_LEN]) -> Self {
        Channel {
            key,
            seq: 0,
            nonce,
        }
    }

    fn record_nonce(&self) -> [u8; 12] {
        // RFC 8439 nonce layout for the channel: the 4 high bytes are zero
        // and the 64-bit sequence number occupies the low bytes.
        let mut n = [0u8; 12];
        n[4..].copy_from_slice(&self.seq.to_le_bytes());
        n
    }

    /// Seal and frame one record: 4-byte LE length, then nonce‖ciphertext‖tag.
    pub fn send(&mut self, stream: &mut impl Write, msg: &Message) -> io::Result<()> {
        let plain = msg.encode();
        let aad = self.seq.to_le_bytes();
        let ct = crypto::aead_seal(&self.key, &self.record_nonce(), &aad, &plain);
        let frame_len = (WIRE_NONCE_LEN + ct.len()) as u32;
        stream.write_all(&frame_len.to_le_bytes())?;
        stream.write_all(&self.nonce)?;
        stream.write_all(&ct)?;
        stream.flush()?;
        self.seq = self.seq.wrapping_add(1);
        Ok(())
    }

    /// Read and open one framed record. A length beyond [`MAX_FRAME`] is a
    /// hard error: the peer is either broken or hostile, and the only sane
    /// response is to drop the connection.
    pub fn recv(&mut self, stream: &mut impl Read) -> io::Result<Message> {
        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf)?;
        let frame_len = u32::from_le_bytes(len_buf) as usize;
        if frame_len < WIRE_NONCE_LEN + 16 || frame_len > MAX_FRAME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "cluster: frame length out of bounds",
            ));
        }
        let mut frame = vec![0u8; frame_len];
        stream.read_exact(&mut frame)?;
        let nonce: [u8; WIRE_NONCE_LEN] = frame[..WIRE_NONCE_LEN].try_into().unwrap();
        if nonce != self.nonce {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "cluster: unexpected record nonce",
            ));
        }
        let aad = self.seq.to_le_bytes();
        let plain = crypto::aead_open(&self.key, &self.record_nonce(), &aad, &frame[WIRE_NONCE_LEN..])
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "cluster: record auth failed"))?;
        self.seq = self.seq.wrapping_add(1);
        Message::decode(&plain).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "cluster: malformed message")
        })
    }
}

/// Exchange wire nonces and derive the two channel directions.
///
/// The client generates `client_nonce`, the server `server_nonce`; each side
/// sends its nonce in cleartext (12 bytes, fixed length, no framing) and
/// reads the peer's. Channel keys come from
/// [`crypto::channel_keys`], so both directions are bound to the PSK, both
/// nonces, and the session. Returns `(read_channel, write_channel)` from
/// this side's perspective.
pub fn handshake(
    stream: &mut (impl Read + Write),
    psk: &[u8; 32],
    is_server: bool,
) -> io::Result<(Channel, Channel)> {
    let mut mine = [0u8; WIRE_NONCE_LEN];
    crate::os_random(&mut mine)?;
    let mut theirs = [0u8; WIRE_NONCE_LEN];
    if is_server {
        stream.read_exact(&mut theirs)?;
        stream.write_all(&mine)?;
        stream.flush()?;
    } else {
        stream.write_all(&mine)?;
        stream.flush()?;
        stream.read_exact(&mut theirs)?;
    }
    let (client_nonce, server_nonce) = if is_server {
        (theirs, mine)
    } else {
        (mine, theirs)
    };
    let (c2s, s2c) = crypto::channel_keys(psk, &client_nonce, &server_nonce);
    let read = if is_server { Channel::new(c2s, client_nonce) } else { Channel::new(s2c, server_nonce) };
    let write = if is_server { Channel::new(s2c, server_nonce) } else { Channel::new(c2s, client_nonce) };
    Ok((read, write))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_roundtrip() {
        let msg = Message::Task {
            id: 7,
            gas: 100_000,
            source: b"40 + 2".to_vec(),
        };
        assert_eq!(Message::decode(&msg.encode()), Some(msg));
    }

    #[test]
    fn result_roundtrip_with_text_and_nonfinite_num() {
        let msg = Message::Result {
            id: u64::MAX,
            status: TaskStatus::Runtime,
            num: f64::from_bits(0x7ff8_0000_dead_beef), // quiet NaN, fixed bits
            text: "[Line 1, Col 3]: nope".as_bytes().to_vec(),
        };
        let back = Message::decode(&msg.encode()).unwrap();
        // NaN != NaN under PartialEq, so compare the exact bit pattern.
        let Message::Result {
            id,
            status,
            num,
            text,
        } = back
        else {
            panic!("expected Result, got {back:?}");
        };
        assert_eq!(id, u64::MAX);
        assert_eq!(status, TaskStatus::Runtime);
        assert_eq!(num.to_bits(), 0x7ff8_0000_dead_beef);
        assert_eq!(text, "[Line 1, Col 3]: nope".as_bytes().to_vec());
    }

    #[test]
    fn value_roundtrip_carries_tag_and_text() {
        for (tag, num, text) in [
            (ValueTag::Number, 42.0, "42"),
            (ValueTag::Bool, 1.0, "true"),
            (ValueTag::String, f64::NAN, "42"),
            (ValueTag::Nil, f64::NAN, "nil"),
            (ValueTag::Undef, f64::NAN, "undefined"),
        ] {
            let msg = Message::Value {
                id: 11,
                tag,
                num,
                text: text.as_bytes().to_vec(),
            };
            let back = Message::decode(&msg.encode()).unwrap();
            let Message::Value {
                id,
                tag: got_tag,
                num: got_num,
                text: got_text,
            } = back
            else {
                panic!("expected Value, got {back:?}");
            };
            assert_eq!(id, 11);
            assert_eq!(got_tag, tag);
            assert_eq!(got_num.to_bits(), num.to_bits());
            assert_eq!(got_text, text.as_bytes());
        }
    }

    #[test]
    fn a_value_tag_outside_the_enum_is_malformed() {
        let mut frame = Message::Value {
            id: 1,
            tag: ValueTag::String,
            num: 0.0,
            text: b"x".to_vec(),
        }
        .encode();
        // The tag sits at offset 9: one tag byte, eight id bytes.
        frame[9..13].copy_from_slice(&99u32.to_le_bytes());
        assert!(Message::decode(&frame).is_none());
    }

    #[test]
    fn ping_pong_and_error_roundtrip() {
        assert_eq!(Message::decode(&Message::Ping.encode()), Some(Message::Ping));
        assert_eq!(Message::decode(&Message::Pong.encode()), Some(Message::Pong));
        let err = Message::Error {
            id: 9,
            code: ErrorCode::GasOverCap,
        };
        assert_eq!(Message::decode(&err.encode()), Some(err));
    }

    #[test]
    fn decode_rejects_garbage_and_trailing_bytes() {
        assert!(Message::decode(&[]).is_none());
        assert!(Message::decode(&[0xee]).is_none());
        let mut bad = Message::Ping.encode();
        bad.push(0);
        assert!(Message::decode(&bad).is_none());
    }

    #[test]
    fn sealed_channel_roundtrip_over_loopback() {
        let psk = b"0123456789abcdef0123456789abcdef";
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let (mut rd, mut wr) = handshake(&mut s, psk, true).unwrap();
            let m = rd.recv(&mut s).unwrap();
            wr.send(&mut s, &m).unwrap();
        });
        let mut c = std::net::TcpStream::connect(addr).unwrap();
        let (mut rd, mut wr) = handshake(&mut c, psk, false).unwrap();
        let task = Message::Task {
            id: 1,
            gas: 0,
            source: b"1 + 1".to_vec(),
        };
        wr.send(&mut c, &task).unwrap();
        assert_eq!(rd.recv(&mut c).unwrap(), task);
        server.join().unwrap();
    }

    #[test]
    fn wrong_psk_fails_record_auth() {
        let psk_a = b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let psk_b = b"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            // The server completes its half; the client's first record must
            // then fail authentication.
            let _ = handshake(&mut s, psk_a, true).map(|(mut rd, mut wr)| {
                let _ = rd.recv(&mut s);
                let _ = wr.send(&mut s, &Message::Pong);
            });
        });
        let mut c = std::net::TcpStream::connect(addr).unwrap();
        let (mut rd, mut wr) = handshake(&mut c, psk_b, false).unwrap();
        wr.send(&mut c, &Message::Ping).unwrap();
        assert!(rd.recv(&mut c).is_err());
        server.join().unwrap();
    }

    #[test]
    fn replayed_record_is_rejected() {
        let psk = b"0123456789abcdef0123456789abcdef";
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        // Intercept: proxy that records the client's first frame and sends
        // it twice to a real server; the second delivery must fail.
        let proxy = std::thread::spawn(move || {
            let (mut upstream, _) = listener.accept().unwrap();
            let server_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let server_addr = server_listener.local_addr().unwrap();
            let server = std::thread::spawn(move || {
                let (mut s, _) = server_listener.accept().unwrap();
                let (mut rd, mut wr) = handshake(&mut s, psk, true).unwrap();
                // First delivery authenticates; second is a replay.
                let _ = rd.recv(&mut s);
                let _ = wr.send(&mut s, &Message::Pong);
                assert!(rd.recv(&mut s).is_err());
            });
            let mut downstream = std::net::TcpStream::connect(server_addr).unwrap();
            // Handshake bytes pass through untouched.
            let mut hs = [0u8; WIRE_NONCE_LEN];
            upstream.read_exact(&mut hs).unwrap();
            downstream.write_all(&hs).unwrap();
            downstream.flush().unwrap();
            downstream.read_exact(&mut hs).unwrap();
            upstream.write_all(&hs).unwrap();
            upstream.flush().unwrap();
            // Read the client's first frame, deliver it twice.
            let mut len = [0u8; 4];
            upstream.read_exact(&mut len).unwrap();
            let mut frame = vec![0u8; u32::from_le_bytes(len) as usize];
            upstream.read_exact(&mut frame).unwrap();
            for _ in 0..2 {
                downstream.write_all(&len).unwrap();
                downstream.write_all(&frame).unwrap();
                downstream.flush().unwrap();
            }
            // Relay the pong so the client's first recv succeeds.
            let mut out_len = [0u8; 4];
            downstream.read_exact(&mut out_len).unwrap();
            let mut out = vec![0u8; u32::from_le_bytes(out_len) as usize];
            downstream.read_exact(&mut out).unwrap();
            upstream.write_all(&out_len).unwrap();
            upstream.write_all(&out).unwrap();
            upstream.flush().unwrap();
            server.join().unwrap();
        });
        let mut c = std::net::TcpStream::connect(addr).unwrap();
        let (mut rd, mut wr) = handshake(&mut c, psk, false).unwrap();
        wr.send(&mut c, &Message::Ping).unwrap();
        assert_eq!(rd.recv(&mut c).unwrap(), Message::Pong);
        proxy.join().unwrap();
    }
}
