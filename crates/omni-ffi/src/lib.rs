//! OmniEngine's C ABI — version 4.
//!
//! This is the only crate permitted to name a `#[repr(C)]` type or an
//! `extern "C"` function, which is what keeps `omni-core` from becoming
//! FFI-shaped by accident.
//!
//! ## The three rules that make this sound
//!
//! **A pointer that crosses the boundary addresses bytes the engine owns for as
//! long as the handle does.** Phase 1 said "no arena pointer crosses the
//! boundary", and Phase 2 replaced that with something narrower and more useful:
//! [`omni_exec`] promotes a string *result* into the egress block before
//! returning, and egress rewinds rather than frees, so its bytes stay mapped for
//! the life of the handle. Host strings injected by [`omni_define_string`] live
//! in their own allocation outside every arena. Nothing in an [`OmniValue`] that
//! leaves this crate addresses the execution arena.
//!
//! The consequence is worth stating out loud: **a stale `OmniValue` is a wrong
//! read, never a dangling one.** A result is valid until the next `omni_exec`,
//! `omni_exec_many`, or `omni_exec_parallel` on the same engine; calling
//! `omni_str_copy` after that copies whatever now sits at those addresses. And
//! because [`omni_str_copy`] re-checks the address against the engine that owns
//! it, a fabricated pointer — or one that belongs to *another* engine, or one
//! whose binding has since been redefined — copies zero bytes rather than reading
//! memory it has no claim on.
//!
//! [`omni_exec_many`] and [`omni_exec_parallel`] are the places where *several*
//! results are live at once. They rewind egress once for the whole batch rather
//! than once per item, so all N string results stay readable together until the
//! next execution of any kind. The price is stated rather than hidden: reserved
//! egress bytes are bounded by one batch's total demand, not by one item's.
//! Parallel workers own forked engines and copy every result back into the
//! caller's egress before they exit, so no worker memory outlives the call.
//!
//! **A Rust panic may never unwind into a host.** Unwinding past `extern "C"` is
//! undefined behaviour, and it is the most common defect in hand-rolled Rust FFI.
//! Every body here runs inside `catch_unwind`, and a caught panic becomes
//! [`OMNI_ERR_PANIC`] rather than a crashed interpreter.
//!
//! **A host callback may never unwind into Rust, and may never alias the engine.**
//! The third rule is new and it is the one that makes the other two survive a
//! callback. An [`OmniEngine`] holds its engine in a [`RefCell`], so the borrow
//! an evaluation runs on is *checked* rather than assumed: a re-entrant call on
//! the same handle meets [`OMNI_ERR_REENTRANT`] instead of creating the second
//! `&mut` that would be undefined behaviour at the cast, before any code could
//! notice. That refusal is what makes two things sound at once — the argument
//! strings a callback may dereference, and the panic boundary, since the only way
//! to get a Rust frame unwinding through a foreign one is to re-enter.
//!
//! The mirror is the callback's own failure. [`omni_host_fn`] reports an error by
//! *returning* [`OMNI_HOST_ERR`] with a message in `out`, never by unwinding: a
//! Python exception cannot cross a ctypes trampoline at all, and a JavaScript
//! throw would unwind through the native frames `omni_exec` is sitting on. Both
//! SDKs therefore catch before Rust ever sees the failure.

use std::cell::{Ref, RefCell, RefMut};
use std::ffi::{c_char, c_double, c_void};
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::slice;

use omni_core::{Engine, ErrorKind, HostError, HostOutcome, OmniError, Value};

/// The ceilings a host has to size its buffers and names against.
///
/// Re-exported rather than restated: they are part of the ABI's contract, and a
/// number duplicated on both sides of a module boundary is a number that drifts.
pub use omni_core::{MAX_ARGS, MAX_CALL_DEPTH, MAX_IDENTIFIER_LEN, MAX_SOURCE_LEN, MAX_STRING_LEN};

/// Items one batch call may carry.
///
/// A boundary bound, not a language one: it caps how many pointers this crate
/// will read from a host array, and with it the size of the per-item error list
/// the handle retains. 4096 programs in one crossing is already far past the
/// point where the crossing cost has been amortised to nothing. The same bound
/// applies to [`omni_exec_many`] and [`omni_exec_parallel`].
pub const MAX_BATCH: usize = 4096;

pub const OMNI_OK: u32 = 0;
pub const OMNI_ERR_COMPILE: u32 = 1;
pub const OMNI_ERR_RUNTIME: u32 = 2;
pub const OMNI_ERR_PANIC: u32 = 3;
pub const OMNI_ERR_ARG: u32 = 4;
/// A host function reported its own failure. Its text is in the engine's last
/// error, and the distinction from [`OMNI_ERR_RUNTIME`] is the point: a host that
/// cannot tell "my callback threw" from "the program divided by zero" cannot
/// route the one to its own error handling.
pub const OMNI_ERR_HOST: u32 = 5;
/// The handle was already inside an evaluation, so this call would have aliased
/// it. Only reachable from a host function that calls back into the engine that
/// is calling it — see the module doc's third rule.
pub const OMNI_ERR_REENTRANT: u32 = 6;

pub const OMNI_NUMBER: u32 = 0;
pub const OMNI_NIL: u32 = 1;
pub const OMNI_BOOL: u32 = 2;
pub const OMNI_UNDEF: u32 = 3;
pub const OMNI_STRING: u32 = 4;

/// The wire format of this ABI, not the crate's version.
///
/// It changes when the layout of a struct or the meaning of an existing
/// signature changes — which is to say, when a host compiled against the old
/// value would read garbage from the new one. Both SDKs call
/// [`omni_abi_version`] at load time and refuse to continue on a mismatch, so a
/// stale `omni.dll` turns into one clear error at import instead of a wrong
/// number ten thousand calls later.
///
/// Version 4 is the batch increment, and like version 3 it changed no layout:
/// `OmniValue` is still 16 bytes at the same offsets and `OmniStats` is still 72.
/// The bump is for [`omni_exec_many`] and [`omni_batch_error`], which a v3 host
/// cannot declare, and for `OmniStats::execs`, which now counts batch items as
/// well as single calls — a v3 host reading it as "number of omni_exec calls"
/// would draw a wrong conclusion from a correct number.
///
/// Version 3 was the callback increment: [`omni_host_fn`], [`OMNI_ERR_HOST`] and
/// [`OMNI_ERR_REENTRANT`], none of which a v2 host could declare or recognise.
pub const OMNI_ABI_VERSION: u32 = 4;

/// The payload of an [`OmniValue`]: whichever arm its tag says is live.
///
/// A union rather than two adjacent fields, so a string costs the same 16 bytes
/// a number does. Both arms are the same width, and every bit pattern of both is
/// a valid value, which is what makes reading the arm matching `tag` free of
/// surprises.
#[repr(C)]
#[derive(Clone, Copy)]
pub union OmniPayload {
    /// Live for [`OMNI_NUMBER`] and [`OMNI_BOOL`].
    pub number: c_double,
    /// Live for [`OMNI_STRING`]. Which of the two it addresses depends on who
    /// wrote the struct, and both rules are in `omni.h`:
    ///
    /// - a result from [`omni_exec`] identifies the string inside a block the
    ///   engine owns — copy it with [`omni_str_copy`], do not `memcpy` from it;
    /// - an argument handed to a host callback is the first byte of its text
    ///   (see [`encode_argument`]), readable for the callback's extent.
    pub ptr: *const c_char,
}

/// Tagged result, 16 bytes: `tag` at 0, `len` at 4, `payload` at 8.
///
/// `len` fills the hole that padding already left between a `u32` tag and an
/// 8-byte payload, which is why adding string support cost no bytes. For
/// [`OMNI_STRING`] it is the byte count the host should reserve; for every other
/// tag it is zero.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct OmniValue {
    pub tag: u32,
    pub len: u32,
    pub payload: OmniPayload,
}

/// Number of bytes of the payload to hand back at most.
///
/// A `u32` length against a 16 MiB ceiling, so the conversion is total; the
/// assert keeps that true if the ceiling ever moves.
const _: () = assert!(MAX_STRING_LEN < u32::MAX as usize);

// Pinned so a Python `ctypes.Structure` or a Node `koffi.struct` that drifts out
// of agreement fails at compile time rather than reading garbage at run time.
const _: () = assert!(std::mem::size_of::<OmniValue>() == 16);
const _: () = assert!(std::mem::size_of::<OmniPayload>() == 8);
const _: () = assert!(std::mem::offset_of!(OmniValue, tag) == 0);
const _: () = assert!(std::mem::offset_of!(OmniValue, len) == 4);
const _: () = assert!(std::mem::offset_of!(OmniValue, payload) == 8);

impl OmniValue {
    /// A number, which is also how a boolean crosses the boundary.
    #[inline]
    pub fn number(&self) -> Option<c_double> {
        if self.tag == OMNI_NUMBER || self.tag == OMNI_BOOL {
            // Safety: `number` is the live arm for these tags, and every bit
            // pattern of an f64 is a value.
            Some(unsafe { self.payload.number })
        } else {
            None
        }
    }

    /// The address a string carries. Reading it is not a dereference, and
    /// [`omni_str_copy`] is the only function that dereferences it.
    #[inline]
    pub fn string_ptr(&self) -> Option<*const c_char> {
        if self.tag == OMNI_STRING {
            // Safety: `ptr` is the live arm for this tag.
            Some(unsafe { self.payload.ptr })
        } else {
            None
        }
    }

    /// Build the string arm, which is the one place a pointer enters the struct.
    fn string(bytes: &[u8], addr: *const u8) -> OmniValue {
        OmniValue {
            tag: OMNI_STRING,
            len: bytes.len() as u32,
            payload: OmniPayload {
                ptr: addr.cast::<c_char>(),
            },
        }
    }
}

impl fmt::Debug for OmniValue {
    /// Prints the arm the tag selects.
    ///
    /// A union cannot know which field is live, and `#[derive(Debug)]` would have
    /// to pick one — printing a `double` over what is really an address is how a
    /// debugger teaches you to distrust it. So the tag decides, and this is also
    /// why `OmniPayload` has no `Debug` of its own.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let payload = match self.tag {
            OMNI_STRING => match self.string_ptr() {
                Some(p) if p.is_null() => "ptr=null".to_string(),
                Some(p) => format!("ptr={p:p} len={}", self.len),
                None => unreachable!("OMNI_STRING always has the pointer arm"),
            },
            other => match self.number() {
                Some(n) => format!("number={n}"),
                None => format!("tag={other} payload=unused"),
            },
        };
        f.debug_struct("OmniValue")
            .field("tag", &self.tag)
            .field("len", &self.len)
            .field("payload", &payload)
            .finish()
    }
}

impl Default for OmniValue {
    /// `undefined`, which is the answer that cannot be mistaken for a result.
    fn default() -> Self {
        OmniValue {
            tag: OMNI_UNDEF,
            len: 0,
            payload: OmniPayload { number: 0.0 },
        }
    }
}

impl PartialEq for OmniValue {
    /// Field-wise, reading only the arm the tag selects.
    ///
    /// Two strings compare by address, because that is all a host can be trusted
    /// with: content equality is `omni-core`'s job, and it needs the bytes.
    fn eq(&self, other: &OmniValue) -> bool {
        if self.tag != other.tag || self.len != other.len {
            return false;
        }
        if self.tag == OMNI_STRING {
            // Safety: both sides were built with `ptr` as the live arm.
            unsafe { self.payload.ptr == other.payload.ptr }
        } else {
            // Safety: both sides were built with `number` as the live arm.
            unsafe { self.payload.number.to_bits() == other.payload.number.to_bits() }
        }
    }
}

/// The memory story, told in counters a host can read without trusting a Rust
/// log line, plus the wall-clock cost of the most recent execution.
///
/// `size` is written by the *callee* and compared by the *caller* against the
/// size it compiled with. A struct whose layout drifted produces a mismatch the
/// host can detect before it reads a field that is no longer where it expects.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OmniStats {
    /// `size_of::<OmniStats>()` as the callee understands it.
    pub size: u32,
    /// Wall-clock microseconds the most recent execution spent in the engine,
    /// lexing included. A failed execution counts too.
    ///
    /// This reuses the four bytes the compiler already padded after `size`, so
    /// the struct stays 72 bytes and every other field keeps its offset — the
    /// reason adding it did not move the ABI. It saturates at `u32::MAX` rather
    /// than wrapping.
    pub last_exec_micros: u32,
    /// Executions started: one per `omni_exec`, one per item of every
    /// `omni_exec_many`. Not a count of crossings — that is what ABI v4 changed
    /// about this field, and why the version moved.
    pub execs: u64,
    pub strings: u64,
    pub arena_bytes_in_use: u64,
    pub arena_bytes_reserved: u64,
    pub arena_chunks: u64,
    pub egress_bytes_in_use: u64,
    pub egress_bytes_reserved: u64,
    pub egress_high_water: u64,
}

const _: () = assert!(std::mem::size_of::<OmniStats>() == 72);
const _: () = assert!(std::mem::align_of::<OmniStats>() == 8);
const _: () = assert!(std::mem::offset_of!(OmniStats, size) == 0);
const _: () = assert!(std::mem::offset_of!(OmniStats, last_exec_micros) == 4);
const _: () = assert!(std::mem::offset_of!(OmniStats, execs) == 8);
const _: () = assert!(std::mem::offset_of!(OmniStats, egress_high_water) == 64);

/// An opaque engine handle.
///
/// Has no `#[repr(C)]` because C never looks inside it, and `Engine` owns its
/// identifier strings, so there is no lifetime that could make a long-lived
/// handle unsound.
///
/// The [`RefCell`] is not decoration. Every export below either takes the engine
/// for reading or for writing, and a host function that called back into
/// [`omni_exec`] would create a second `&mut Engine` from the same raw pointer —
/// undefined behaviour at the cast, before any check could run. With the cell,
/// that call is a `try_borrow_mut` that fails, and the re-entrancy becomes a
/// status code. The cost is one integer read per call, inside a function that
/// already pays for `catch_unwind`.
pub struct OmniEngine {
    inner: RefCell<Engine>,
    /// The most recent batch's per-item errors, in source order, `None` for an
    /// item that succeeded. Retained so [`omni_batch_error`] can name the failure
    /// at index 3 of 256 — the engine's own `last_error` only holds the final
    /// item's, which is the wrong one for every other index.
    ///
    /// Its own cell rather than a field beside `Engine`, so writing it never
    /// needs a `&mut OmniEngine` while a borrow derived from the same handle is
    /// live. Cleared by `omni_exec`: after a single execution there is no current
    /// batch, and a stale message would be worse than none.
    batch: RefCell<Vec<Option<OmniError>>>,
}

/// The callback a host registers, and the only shape a host function may have.
///
/// `Option` because C's null is then `None` in the type: an absent callback is
/// unmatched rather than a pointer that will be called and crash.
///
/// `args` points at `argc` [`OmniValue`]s whose string arms address this engine's
/// own memory. That is a **borrowed view, valid until this function returns**:
/// copy what you need, and never store the address. It is sound for exactly as
/// long as the arena cannot reset under the call, and the arena cannot reset
/// because `arena.reset()` runs only at the entry of `omni_exec` — which a
/// re-entrant call cannot reach.
///
/// Return [`OMNI_HOST_OK`] with the value in `out`, or [`OMNI_HOST_ERR`] with the
/// message in `out` as an `OMNI_STRING`. A nonzero return is data, and it is the
/// only way a host may report failure: nothing may unwind into Rust from here.
pub type OmniHostFn = Option<
    unsafe extern "C" fn(
        user_data: *mut c_void,
        args: *const OmniValue,
        argc: u32,
        out: *mut OmniValue,
    ) -> i32,
>;

/// The status a host function returns for success and for failure.
pub const OMNI_HOST_OK: i32 = 0;
/// The host failed; whatever `out` carries is the message, not a result.
pub const OMNI_HOST_ERR: i32 = 1;

/// `user_data` as the engine stores it.
///
/// A raw pointer is not `Send`, and the registry lives inside an `Engine` that
/// is, so without this newtype the callback would silently make the handle
/// non-`Send` — a change nothing in this crate would notice until someone moved
/// a handle across threads. The assertion restates the rule the whole ABI already
/// runs on: one handle, one thread, and the host keeps the pointee alive at least
/// as long as the registration.
#[derive(Clone, Copy)]
struct HostContext(*mut c_void);

// Safety: the pointer is only ever dereferenced by the host that supplied it,
// while the engine's borrow is held. `Sync` extends the existing contract to
// `omni_exec_parallel`: a registered callback may run on several worker threads
// at once, so whatever `user_data` addresses must itself be safe to use from
// those threads. A host that cannot promise this must not call the parallel
// entry point, or must not register that callback.
unsafe impl Send for HostContext {}
unsafe impl Sync for HostContext {}

fn status_for(kind: ErrorKind) -> u32 {
    match kind {
        ErrorKind::Lex | ErrorKind::Parse => OMNI_ERR_COMPILE,
        ErrorKind::Runtime => OMNI_ERR_RUNTIME,
        ErrorKind::Panic => OMNI_ERR_PANIC,
        ErrorKind::Host => OMNI_ERR_HOST,
    }
}

/// Take the handle for writing, or report the re-entrancy that would have
/// aliased it.
///
/// The argument is a *shared* reference on purpose: a `RefCell` hands out `RefMut`
/// through `&`, which is the one way to get a checked mutable borrow out of a
/// pointer that cannot lend a lifetime. A raw pointer could not appear here and
/// keep the returned borrow honest about how long it lives.
fn borrow_mut(engine: &OmniEngine) -> Result<RefMut<'_, Engine>, u32> {
    engine
        .inner
        .try_borrow_mut()
        .map_err(|_| OMNI_ERR_REENTRANT)
}

/// Take the handle for reading. Same contract as [`borrow_mut`], for the exports
/// that must not stop a host from asking a question mid-evaluation.
fn borrow(engine: &OmniEngine) -> Result<Ref<'_, Engine>, u32> {
    engine.inner.try_borrow().map_err(|_| OMNI_ERR_REENTRANT)
}

/// Run `body`, converting a panic into [`OMNI_ERR_PANIC`].
#[inline]
fn guarded<F: FnOnce() -> u32>(body: F) -> u32 {
    match catch_unwind(AssertUnwindSafe(body)) {
        Ok(code) => code,
        Err(_) => OMNI_ERR_PANIC,
    }
}

/// Run `body` for a function whose result is a count rather than a status.
///
/// A caught panic reports `usize::default()` — zero, the same answer as "nothing
/// to copy". Deliberately not a random number: `omni_str_copy` already means
/// "zero bytes" for a value it cannot account for, and a panic says the same
/// thing for the same reason.
#[inline]
fn guarded_len<F: FnOnce() -> usize>(body: F) -> usize {
    catch_unwind(AssertUnwindSafe(body)).unwrap_or_default()
}

/// Put a core value into the boundary struct, reading string content through the
/// engine that owns it.
fn encode(engine: &Engine, v: Value) -> OmniValue {
    if v.is_string() {
        // The promotion rule inside `omni_exec` guarantees a string that reaches
        // here addresses a block this engine owns. If that ever fails, the
        // honest answer is an empty string carrying no address: a host cannot
        // dereference a pointer we refuse to dereference ourselves, and
        // `omni_str_copy` copies zero bytes and says so.
        let bytes = engine.string_of(v).unwrap_or_default();
        let addr = if bytes.is_empty() {
            ptr::null()
        } else {
            v.as_ptr().unwrap_or(ptr::null())
        };
        return OmniValue::string(bytes, addr);
    }
    if let Some(n) = v.as_f64() {
        return OmniValue {
            tag: OMNI_NUMBER,
            len: 0,
            payload: OmniPayload { number: n },
        };
    }
    if let Some(b) = v.as_bool() {
        return OmniValue {
            tag: OMNI_BOOL,
            len: 0,
            payload: OmniPayload {
                number: f64::from(u32::from(b)),
            },
        };
    }
    let tag = if v == Value::UNDEF {
        OMNI_UNDEF
    } else {
        OMNI_NIL
    };
    OmniValue {
        tag,
        len: 0,
        payload: OmniPayload { number: 0.0 },
    }
}

/// Put one *call argument* into the boundary struct.
///
/// One difference from [`encode`], and it is the whole of the callback's borrowed
/// view: a string's `payload.ptr` addresses its bytes, not the block header that
/// [`omni_str_copy`] reads. A host dereferences an argument directly —
/// `memcpy(dst, args[i].payload.ptr, args[i].len)` — because the copy helper is
/// unavailable there: it takes the handle, and the handle is already borrowed by
/// the very call that is handing the argument over. The bytes cannot move while
/// the callback runs, because nothing that resets the arena can run while this
/// borrow is held — which is exactly what the header's re-entrancy rule buys.
fn encode_argument(engine: &Engine, v: Value) -> OmniValue {
    if v.is_string() {
        // The same empty-string rule as `encode`: no address is handed out for
        // nothing, so a host that sees a null pointer sees a length of zero too.
        let bytes = engine.string_of(v).unwrap_or_default();
        let addr = if bytes.is_empty() {
            ptr::null()
        } else {
            bytes.as_ptr()
        };
        return OmniValue::string(bytes, addr);
    }
    encode(engine, v)
}

/// Read a string an [`OmniValue`] carries out of *host* memory.
///
/// This is the one place the boundary reads a pointer the engine did not mint, so
/// it is deliberately narrow: `None` when the address is null with a nonzero
/// length or when the length is past [`MAX_STRING_LEN`], both of which the caller
/// turns into a failure rather than a partial read. The bytes are copied into an
/// owned buffer, which is what keeps the engine from ever storing a host address —
/// the same rule [`omni_define_string`] states from the other side.
fn host_bytes(value: &OmniValue) -> Option<Vec<u8>> {
    let len = usize::try_from(value.len).ok()?;
    if len > MAX_STRING_LEN {
        return None;
    }
    let addr = value.string_ptr()?;
    if addr.is_null() {
        return (len == 0).then(Vec::new);
    }
    // Safety: the host guarantees `out` addresses readable bytes for `len`, and
    // this runs inside the dynamic extent of the callback that filled it.
    Some(unsafe { slice::from_raw_parts(addr.cast::<u8>(), len) }.to_vec())
}

/// Lower the arguments, run one host callback, and read its answer back.
///
/// `engine` is the evaluator's own borrow, shared rather than mutable, which is
/// what lets [`encode`] hand the host an address into the arena: those bytes
/// cannot move, because nothing that resets the arena can run while this borrow
/// is held. A returned string is copied out of host memory before the callback's
/// frame is gone, and only then boxed.
fn call_host(
    engine: &Engine,
    callback: OmniHostFn,
    context: HostContext,
    args: &[Value],
) -> HostOutcome {
    let Some(f) = callback else {
        return HostOutcome::Failure(b"the registered callback is null".to_vec());
    };
    if args.len() > MAX_ARGS {
        // Core enforces arity at registration and at the call site, so reaching
        // here means that invariant broke. Refusing beats writing past `buf`.
        return HostOutcome::Failure(b"more arguments than the ABI carries".to_vec());
    }
    let mut buf = [OmniValue::default(); MAX_ARGS];
    for (slot, value) in buf.iter_mut().zip(args) {
        *slot = encode_argument(engine, *value);
    }
    let mut out = OmniValue::default();
    let argc = args.len() as u32;
    // Safety: `f` is the host's own function, registered by name on this thread;
    // `buf`, `out`, and the null `user_data` are all the host agreed to accept.
    let code = unsafe { f(context.0, buf.as_ptr(), argc, &mut out) };
    if code != OMNI_HOST_OK {
        // The host's message if it sent one. A failure with no string attached is
        // still a failure, and core prints the function's name for it.
        return HostOutcome::Failure(host_bytes(&out).unwrap_or_default());
    }
    match out.tag {
        OMNI_NUMBER => match out.number() {
            Some(n) => HostOutcome::Number(n),
            None => HostOutcome::Failure(b"a number result has no readable payload".to_vec()),
        },
        OMNI_BOOL => match out.number() {
            // A host that returns `true` as 1.0 and one that returns 1.0 as `true`
            // are the same eight bytes, so nonzero is the rule and it is stated.
            Some(n) => HostOutcome::Bool(n != 0.0),
            None => HostOutcome::Failure(b"a boolean result has no readable payload".to_vec()),
        },
        OMNI_NIL => HostOutcome::Nil,
        OMNI_UNDEF => HostOutcome::Undefined,
        OMNI_STRING => match host_bytes(&out) {
            Some(bytes) => HostOutcome::String(bytes),
            None => HostOutcome::Failure(b"a string result has no readable bytes".to_vec()),
        },
        other => HostOutcome::Failure(
            format!("the host returned tag {other}, which is not a value").into_bytes(),
        ),
    }
}

/// ABI level of the loaded library. Compare with your `OMNI_ABI_VERSION` before
/// trusting any other symbol.
#[unsafe(no_mangle)]
pub extern "C" fn omni_abi_version() -> u32 {
    OMNI_ABI_VERSION
}

/// Static, NUL-terminated crate version. Never freed.
#[unsafe(no_mangle)]
pub extern "C" fn omni_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0")
        .as_ptr()
        .cast::<c_char>()
}

/// The manifest describes this build or an older one.
pub const OMNI_UPDATE_UP_TO_DATE: i32 = 0;
/// The manifest describes a newer build on the same ABI.
pub const OMNI_UPDATE_AVAILABLE: i32 = 1;
/// The manifest describes a build on a different ABI, so it is not an upgrade
/// this library can take.
pub const OMNI_UPDATE_ABI_MISMATCH: i32 = 2;
/// The bytes are not a manifest this ABI accepts.
pub const OMNI_UPDATE_INVALID: i32 = 3;
/// A null manifest with a nonzero length, or bytes that are not UTF-8.
pub const OMNI_UPDATE_BAD_ARG: i32 = -1;

/// Compare a release manifest with the running library.
///
/// The caller fetches the manifest; this function only reads it. That split is
/// deliberate: the engine has no network stack and no business growing one, and
/// a host that already performs HTTPS should not be made to route it through an
/// interpreter.
///
/// `manifest` is `len` bytes of UTF-8 and need not be NUL-terminated. When the
/// document parses, the version it names is copied into `out_version` as a
/// NUL-terminated string, truncated to `out_cap - 1` bytes, and the return value
/// is [`OMNI_UPDATE_UP_TO_DATE`], [`OMNI_UPDATE_AVAILABLE`] or
/// [`OMNI_UPDATE_ABI_MISMATCH`]. A null `out_version` or a `out_cap` of zero is
/// accepted: the status is still the answer.
///
/// Returns [`OMNI_UPDATE_INVALID`] when the bytes are not a release manifest,
/// [`OMNI_UPDATE_BAD_ARG`] for a malformed argument, and [`OMNI_UPDATE_INVALID`]
/// if a panic escapes the parser.
///
/// # Safety
///
/// `manifest` must be readable for `len` bytes, or be null with a zero length.
/// `out_version` must be writable for `out_cap` bytes, or be null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn omni_check_update(
    manifest: *const c_char,
    len: usize,
    out_version: *mut c_char,
    out_cap: usize,
) -> i32 {
    if len > omni_core::update::MAX_MANIFEST_LEN || (manifest.is_null() && len != 0) {
        return OMNI_UPDATE_BAD_ARG;
    }
    let text = if len == 0 {
        ""
    } else {
        // Safety: the null check above ran and the caller guarantees `manifest`
        // is readable for `len` bytes.
        let bytes = unsafe { slice::from_raw_parts(manifest.cast::<u8>(), len) };
        match std::str::from_utf8(bytes) {
            Ok(text) => text,
            Err(_) => return OMNI_UPDATE_BAD_ARG,
        }
    };
    catch_unwind(AssertUnwindSafe(|| {
        let Ok(parsed) = omni_core::update::parse_manifest(text) else {
            return OMNI_UPDATE_INVALID;
        };
        if !out_version.is_null() && out_cap > 0 {
            let bytes = parsed.version.as_bytes();
            let n = bytes.len().min(out_cap - 1);
            // Safety: `out_version` is writable for `out_cap` bytes and n < out_cap,
            // so the copy and the terminator at out_version[n] both stay inside it.
            unsafe {
                ptr::copy_nonoverlapping(bytes.as_ptr(), out_version.cast::<u8>(), n);
                *out_version.add(n) = 0;
            }
        }
        omni_core::update::check_update(env!("CARGO_PKG_VERSION"), OMNI_ABI_VERSION, &parsed).code()
    }))
    .unwrap_or(OMNI_UPDATE_INVALID)
}

/// Allocate an engine. Returns null if allocation itself panicked.
#[unsafe(no_mangle)]
pub extern "C" fn omni_engine_new() -> *mut OmniEngine {
    match catch_unwind(AssertUnwindSafe(|| {
        Box::into_raw(Box::new(OmniEngine {
            inner: RefCell::new(Engine::new()),
            batch: RefCell::new(Vec::new()),
        }))
    })) {
        Ok(handle) => handle,
        Err(_) => ptr::null_mut(),
    }
}

/// Release an engine, and with it every host string and host function it owns.
///
/// Refusing is the interesting case. A host function that freed its own engine
/// from inside the callback would drop the `Engine` while the evaluator's borrow
/// of it is still live, and the borrow would then be reading freed memory on the
/// way out — so a handle that is mid-evaluation is not freed at all. This returns
/// without doing anything; there is no status to report, and a host that wants
/// one checks [`omni_host_fn_count`] is zero and calls it from outside a callback.
///
/// # Safety
///
/// `engine` must be null or a value returned by [`omni_engine_new`], must not be
/// used after this call, and must not be freed twice.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn omni_engine_free(engine: *mut OmniEngine) {
    if engine.is_null() {
        return;
    }
    // The re-entrancy rule, applied to the one call that cannot report it.
    if borrow_mut(unsafe { &*engine }).is_err() {
        return;
    }
    // Safety: the caller asserts this pointer came from Box::into_raw above
    // and has not been reclaimed already. The borrow above is dropped here, so
    // nothing aliases the engine from this point.
    drop(unsafe { Box::from_raw(engine) });
}

/// Evaluate `source` and write the result to `out`.
///
/// A `len` of zero is a valid empty program, and it does not require a non-null
/// `src`: several hosts hand an empty string or buffer over as a null pointer.
///
/// When the result is a string, `out.payload.ptr` addresses the engine's egress
/// block and stays readable until the next `omni_exec` or [`omni_exec_many`] on
/// this engine — copy it out with [`omni_str_copy`] before then if you intend to
/// keep it.
///
/// # Safety
///
/// `engine` must come from [`omni_engine_new`]. `src` must be readable for
/// `len` bytes and must be UTF-8 (a `len` beyond the allocation is rejected by
/// the size bound but is still undefined behaviour). `out` must be writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn omni_exec(
    engine: *mut OmniEngine,
    src: *const c_char,
    len: usize,
    out: *mut OmniValue,
) -> u32 {
    if engine.is_null() || out.is_null() || len > MAX_SOURCE_LEN {
        return OMNI_ERR_ARG;
    }
    if src.is_null() && len != 0 {
        return OMNI_ERR_ARG;
    }
    // A zero-length slice from a null pointer is undefined behaviour even
    // though it reads nothing, so the empty program never touches `src`.
    let text = if len == 0 {
        ""
    } else {
        // Safety: the null check and the caller's guarantee that `src` is
        // readable for `len` bytes both hold here.
        let bytes = unsafe { slice::from_raw_parts(src.cast::<u8>(), len) };
        match std::str::from_utf8(bytes) {
            Ok(text) => text,
            // Malformed input is an argument problem, not a compile error, so the
            // host can tell "you called me wrong" from "your program is wrong".
            Err(_) => return OMNI_ERR_ARG,
        }
    };
    // Safety: the null check above ran, and the caller guarantees the handle is
    // live. One handle, one thread.
    let handle = unsafe { &*engine };
    let mut engine = match borrow_mut(handle) {
        Ok(borrow) => borrow,
        // A host function calling `omni_exec` on the engine that is calling it.
        // Refused rather than aliased — see the module doc's third rule.
        Err(code) => return code,
    };
    guarded(|| {
        // A single execution ends the batch's currency: `omni_batch_error` must
        // not answer with a message from a call the host has moved on from.
        handle.batch.borrow_mut().clear();
        match engine.exec(text) {
            Ok(value) => {
                // Safety: `out` was checked non-null and writable.
                unsafe { *out = encode(&engine, value) };
                OMNI_OK
            }
            Err(error) => status_for(error.kind),
        }
    })
}

/// Read and validate `count` sources from caller-owned arrays.
///
/// Every source is validated before any of them runs, so a malformed item refuses
/// the whole call with nothing written — the same answer [`omni_exec`] gives for
/// bad UTF-8, rather than a half-executed batch the host built wrong.
///
/// # Safety
///
/// `sources` must be readable for `count` pointers and `lengths` for `count`
/// `size_t`s; each `sources[i]` must be readable for `lengths[i]` bytes and be
/// UTF-8, or be null when that length is zero. The returned slices borrow that
/// memory for as long as the caller keeps it alive and unmodified.
unsafe fn read_sources<'a>(
    sources: *const *const c_char,
    lengths: *const usize,
    count: usize,
) -> Result<Vec<&'a str>, u32> {
    if sources.is_null() || lengths.is_null() {
        return Err(OMNI_ERR_ARG);
    }
    // Safety: the caller guarantees `count` readable elements in each array.
    let ptrs = unsafe { slice::from_raw_parts(sources, count) };
    let lens = unsafe { slice::from_raw_parts(lengths, count) };
    let mut texts = Vec::with_capacity(count);
    for (&src, &len) in ptrs.iter().zip(lens) {
        if len > MAX_SOURCE_LEN || (src.is_null() && len != 0) {
            return Err(OMNI_ERR_ARG);
        }
        let text = if len == 0 {
            ""
        } else {
            // Safety: as above, `src` is readable for `len` bytes.
            let bytes = unsafe { slice::from_raw_parts(src.cast::<u8>(), len) };
            match std::str::from_utf8(bytes) {
                Ok(text) => text,
                Err(_) => return Err(OMNI_ERR_ARG),
            }
        };
        texts.push(text);
    }
    Ok(texts)
}

/// Evaluate `count` programs in one crossing, writing one value and one status
/// per item into caller-owned arrays.
///
/// Returns [`OMNI_OK`] when every item succeeded, and otherwise the status of the
/// **first** item that failed. The per-item answers are in `statuses` either way:
/// a failing item does not stop the batch, so `out[i]` and `statuses[i]` are
/// written for every `i` in `0..count`. A failed item's value is [`OMNI_UNDEF`],
/// and its message is available from [`omni_batch_error`] at that index.
///
/// All N string results are readable together when this returns, until the next
/// `omni_exec` or `omni_exec_many` on the same engine. That is the capability a
/// loop of single calls cannot offer, and it is why egress rewinds once here.
///
/// `count` of zero is a no-op returning [`OMNI_OK`]: nothing is written, nothing
/// is reclaimed, and `sources` and `lengths` may then be null. A host polling an
/// empty queue does not expire the results it is still holding.
///
/// # Safety
///
/// `engine` must come from [`omni_engine_new`]. `sources` must be readable for
/// `count` pointers and `lengths` for `count` `size_t`s; each `sources[i]` must
/// be readable for `lengths[i]` bytes and be UTF-8, or be null when that length
/// is zero. `out` must be writable for `count` [`OmniValue`]s and `statuses` for
/// `count` `uint32_t`s. A `lengths[i]` beyond the allocation is rejected by the
/// size bound but is still undefined behaviour.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn omni_exec_many(
    engine: *mut OmniEngine,
    sources: *const *const c_char,
    lengths: *const usize,
    count: usize,
    out: *mut OmniValue,
    statuses: *mut u32,
) -> u32 {
    if engine.is_null() || out.is_null() || statuses.is_null() || count > MAX_BATCH {
        return OMNI_ERR_ARG;
    }
    if count == 0 {
        return OMNI_OK;
    }
    // Safety: the null checks ran and the caller guarantees a live handle.
    let handle = unsafe { &*engine };
    let mut engine = match borrow_mut(handle) {
        Ok(borrow) => borrow,
        Err(code) => return code,
    };
    guarded(|| {
        // Safety: `count` is bounded and the caller guarantees readable arrays.
        let texts = match unsafe { read_sources(sources, lengths, count) } {
            Ok(texts) => texts,
            Err(code) => return code,
        };

        let items = engine.exec_many(&texts);
        let mut batch = handle.batch.borrow_mut();
        batch.clear();
        let mut first = OMNI_OK;
        for (i, item) in items.iter().enumerate() {
            let status = match &item.error {
                Some(error) => status_for(error.kind),
                None => OMNI_OK,
            };
            // Safety: `out` and `statuses` are writable for `count` elements and
            // `i < count`, because core returns one item per source.
            unsafe {
                *out.add(i) = encode(&engine, item.value);
                *statuses.add(i) = status;
            }
            batch.push(item.error.clone());
            if first == OMNI_OK {
                first = status;
            }
        }
        first
    })
}

/// Evaluate `count` programs across a worker group, writing one value and one
/// status per item into caller-owned arrays.
///
/// This is [`omni_exec_many`] with the evaluation spread over up to `max_threads`
/// forked engines. The call is **synchronous**: it returns only when every item
/// has finished. Results are written in source order, every item gets a status,
/// and the per-item errors are available from [`omni_batch_error`] exactly as
/// after a sequential batch. All N string results are readable together until the
/// next `omni_exec`, `omni_exec_many`, or `omni_exec_parallel` on this engine.
///
/// `max_threads == 0` asks the engine for its automatic choice
/// (`available_parallelism`, bounded by `count` and 64). `max_threads == 1` runs
/// the sequential path, byte for byte. A failing or gas-exhausted item never stops
/// its neighbours.
///
/// The handle itself is still borrowed for the duration; this is the supported
/// way to use several cores through one handle, not a licence to call two exports
/// on the same handle concurrently. Host functions registered on the engine may
/// run on worker threads, so anything their `user_data` addresses must be safe to
/// use from those threads.
///
/// # Safety
///
/// `engine` must come from [`omni_engine_new`]. `sources` must be readable for
/// `count` pointers and `lengths` for `count` `size_t`s; each `sources[i]` must
/// be readable for `lengths[i]` bytes and be UTF-8, or be null when that length
/// is zero. `out` must be writable for `count` [`OmniValue`]s and `statuses` for
/// `count` `uint32_t`s. A `lengths[i]` beyond the allocation is rejected by the
/// size bound but is still undefined behaviour.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn omni_exec_parallel(
    engine: *mut OmniEngine,
    sources: *const *const c_char,
    lengths: *const usize,
    count: usize,
    max_threads: u32,
    out: *mut OmniValue,
    statuses: *mut u32,
) -> u32 {
    if engine.is_null() || out.is_null() || statuses.is_null() || count > MAX_BATCH {
        return OMNI_ERR_ARG;
    }
    if count == 0 {
        return OMNI_OK;
    }
    // Safety: the null checks ran and the caller guarantees a live handle.
    let handle = unsafe { &*engine };
    let mut engine = match borrow_mut(handle) {
        Ok(borrow) => borrow,
        Err(code) => return code,
    };
    guarded(|| {
        // Safety: `count` is bounded and the caller guarantees readable arrays.
        let texts = match unsafe { read_sources(sources, lengths, count) } {
            Ok(texts) => texts,
            Err(code) => return code,
        };

        let items = engine.exec_parallel(&texts, max_threads as usize);
        let mut batch = handle.batch.borrow_mut();
        batch.clear();
        let mut first = OMNI_OK;
        for (i, item) in items.iter().enumerate() {
            let status = match &item.error {
                Some(error) => status_for(error.kind),
                None => OMNI_OK,
            };
            // Safety: `out` and `statuses` are writable for `count` elements and
            // `i < count`, because core returns one item per source.
            unsafe {
                *out.add(i) = encode(&engine, item.value);
                *statuses.add(i) = status;
            }
            batch.push(item.error.clone());
            if first == OMNI_OK {
                first = status;
            }
        }
        first
    })
}

/// Copy the rendered error of one item of the most recent batch into `buf`.
///
/// Writes at most `cap - 1` bytes and always NUL-terminates, exactly as
/// [`omni_last_error`] does, and returns the bytes written excluding the
/// terminator. Returns 0 — writes nothing — when `index` is past the batch, when
/// that item succeeded, when no batch is current (a single `omni_exec` ends its
/// currency), when any pointer is null or `cap` is 0, and when called from inside
/// a callback on this engine.
///
/// # Safety
///
/// `engine` must come from [`omni_engine_new`] or be null. `buf` must be
/// writable for `cap` bytes or be null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn omni_batch_error(
    engine: *const OmniEngine,
    index: usize,
    buf: *mut c_char,
    cap: usize,
) -> usize {
    if engine.is_null() || buf.is_null() || cap == 0 {
        return 0;
    }
    // Safety: checked non-null; the caller guarantees a live handle.
    let handle = unsafe { &*engine };
    // The same guard `omni_last_error` takes, and for the same reason: mid-
    // evaluation the batch on record is the *previous* one, and answering from
    // it would hand a callback a message about a call it is not inside of.
    let _guard = match borrow(handle) {
        Ok(guard) => guard,
        Err(_) => return 0,
    };
    let batch = handle.batch.borrow();
    let Some(Some(error)) = batch.get(index) else {
        return 0;
    };
    let rendered = error.to_string();
    let message = rendered.as_bytes();
    let n = message.len().min(cap - 1);
    // Safety: `buf` holds `cap` writable bytes and n < cap, so the copy and the
    // terminator at buf[n] both stay inside it.
    unsafe {
        ptr::copy_nonoverlapping(message.as_ptr(), buf.cast::<u8>(), n);
        *buf.add(n) = 0;
    }
    n
}

/// Evaluate `source`, requiring the result to be a number.
///
/// A string result is `OMNI_ERR_RUNTIME`, exactly as in ABI v1: a host that
/// wants a string calls [`omni_exec`] and reads the tag.
///
/// # Safety
///
/// As [`omni_exec`], with `out` a writable `double`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn omni_exec_number(
    engine: *mut OmniEngine,
    src: *const c_char,
    len: usize,
    out: *mut c_double,
) -> u32 {
    if engine.is_null() || out.is_null() || len > MAX_SOURCE_LEN || (src.is_null() && len != 0) {
        return OMNI_ERR_ARG;
    }
    let mut value = OmniValue::default();
    // Safety: the checks above hold, and `value` is a live local.
    let code = unsafe { omni_exec(engine, src, len, &mut value) };
    if code != OMNI_OK {
        return code;
    }
    let Some(number) = value.number() else {
        return OMNI_ERR_RUNTIME;
    };
    // Safety: checked non-null and writable.
    unsafe { *out = number };
    OMNI_OK
}

/// Copy a string result into storage the caller owns.
///
/// Writes `min(len, cap)` bytes and returns the number written. It does **not**
/// NUL-terminate: an OmniEngine string may contain a NUL, and `out->len` already
/// tells you where the payload ends. To get a C string, allocate `len + 1` and
/// write the terminator yourself.
///
/// Returns 0 — writes nothing, reads nothing — when `value` is not an
/// `OMNI_STRING`, when any pointer is null, when the address is outside the
/// 48 bits the value format can carry, or when the address belongs to something
/// other than `engine`: another engine's arena, a host string whose binding has
/// since been redefined, or a number the caller invented. That last check is the
/// whole reason this function exists instead of a plain `memcpy` at the call
/// site.
///
/// `out->len` is a hint for sizing a buffer; the engine's own header decides how
/// many bytes are actually copied, so a stale length cannot make this read past
/// the end of a block.
///
/// # Safety
///
/// `engine` must come from [`omni_engine_new`], or be null. `value` must be
/// readable as an `OmniValue` or be null. `buf` must be writable for `cap`
/// bytes, or be null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn omni_str_copy(
    engine: *const OmniEngine,
    value: *const OmniValue,
    buf: *mut c_char,
    cap: usize,
) -> usize {
    if engine.is_null() || value.is_null() || buf.is_null() || cap == 0 {
        return 0;
    }
    // Safety: the null checks above ran, and the caller guarantees the handle is
    // live and that `buf` holds `cap` writable bytes.
    let engine = match borrow(unsafe { &*engine }) {
        Ok(borrow) => borrow,
        // Mid-evaluation, so the arena these addresses live in is exactly what is
        // being written. A `usize` cannot carry a status, and zero already means
        // "this is not mine to read", so the refusal reuses the existing answer.
        Err(_) => return 0,
    };
    // Safety: `value` was checked non-null and readable as an `OmniValue`.
    let value = unsafe { &*value };
    let Some(addr) = value.string_ptr() else {
        return 0;
    };
    if addr.is_null() {
        return 0;
    }
    // 48 address bits is what a NaN box can hold, so a pointer with anything in
    // the high bits cannot be one this engine handed out. Checking it here rather
    // than letting `Value::from_ptr` mask it off keeps the debug assert out of
    // the host's error path.
    if (addr as u64) >> 48 != 0 {
        return 0;
    }
    guarded_len(|| {
        // The ownership proof: `string_of` reads the bytes only if the address
        // falls inside this engine's egress block, its execution arena, or one of
        // its live host bindings.
        let Some(bytes) = engine.string_of(Value::from_ptr(addr.cast::<u8>())) else {
            return 0;
        };
        let n = bytes.len().min(cap);
        // Safety: `buf` is writable for `cap` bytes and `n <= cap`.
        unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), buf.cast::<u8>(), n) };
        n
    })
}

/// Bind `name` to a string the host owns.
///
/// The engine copies `bytes` into storage outside every arena, so the binding
/// survives executions and `bytes` itself may be freed as soon as this returns.
/// Redefining `name` replaces the binding and releases the previous bytes; a
/// value the host kept from before then copies out as zero bytes rather than
/// reading freed memory.
///
/// Returns [`OMNI_OK`] or [`OMNI_ERR_ARG`]. The reasons for a rejection are
/// distinguishable in Rust (see `omni_core::HostError`) and collapse to one
/// status here, because they are all "the host called this wrongly": an empty or
/// over-long name, non-UTF-8 in either buffer, or a payload over
/// [`MAX_STRING_LEN`].
///
/// # Safety
///
/// `engine` must come from [`omni_engine_new`]. `name` must be readable for
/// `name_len` bytes and `bytes` for `bytes_len`, or be null with a zero length.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn omni_define_string(
    engine: *mut OmniEngine,
    name: *const c_char,
    name_len: usize,
    bytes: *const c_char,
    bytes_len: usize,
) -> u32 {
    if engine.is_null() {
        return OMNI_ERR_ARG;
    }
    // Checked before any slice is fabricated, so an absurd length is a rejection
    // rather than an out-of-bounds read waiting to happen.
    if name_len > MAX_IDENTIFIER_LEN || bytes_len > MAX_STRING_LEN {
        return OMNI_ERR_ARG;
    }
    if (name.is_null() && name_len != 0) || (bytes.is_null() && bytes_len != 0) {
        return OMNI_ERR_ARG;
    }
    // Safety: the lengths are bounded, non-null, and the caller guarantees both
    // regions are readable for them.
    let (name, payload) = unsafe {
        let name_bytes = if name_len == 0 {
            &[]
        } else {
            slice::from_raw_parts(name.cast::<u8>(), name_len)
        };
        let payload_bytes = if bytes_len == 0 {
            &[]
        } else {
            slice::from_raw_parts(bytes.cast::<u8>(), bytes_len)
        };
        (name_bytes, payload_bytes)
    };
    let Ok(name) = std::str::from_utf8(name) else {
        return OMNI_ERR_ARG;
    };
    // Safety: the null check above ran and the caller guarantees the handle is
    // live. One handle, one thread.
    let mut engine = match borrow_mut(unsafe { &*engine }) {
        Ok(borrow) => borrow,
        Err(code) => return code,
    };
    guarded(|| match engine.define_string(name, payload) {
        Ok(()) => OMNI_OK,
        // Every `HostError` is the caller's fault, so there is nothing else to
        // report and no new status code to teach every host in the repository.
        Err(HostError::EmptyName)
        | Err(HostError::NameTooLong)
        | Err(HostError::NotUtf8)
        | Err(HostError::TooLong)
        | Err(HostError::ArityTooLarge) => OMNI_ERR_ARG,
    })
}

/// Fill `out` with the engine's memory counters.
///
/// Writes `OMNI_STATS_SIZE` into `out->size` first, so a host compiled against a
/// different layout notices before it reads a field that has moved. Returns
/// [`OMNI_OK`], or [`OMNI_ERR_ARG`] when `engine` or `out` is null.
///
/// # Safety
///
/// `engine` must come from [`omni_engine_new`]; `out` must be writable for
/// `size_of::<OmniStats>()` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn omni_debug_stats(engine: *mut OmniEngine, out: *mut OmniStats) -> u32 {
    if engine.is_null() || out.is_null() {
        return OMNI_ERR_ARG;
    }
    let stats = {
        // Safety: the null check above ran, and the caller guarantees the handle
        // is live.
        let engine = match borrow(unsafe { &*engine }) {
            Ok(borrow) => borrow,
            Err(code) => return code,
        };
        let s = engine.stats();
        OmniStats {
            size: std::mem::size_of::<OmniStats>() as u32,
            last_exec_micros: s.last_exec_micros,
            execs: s.execs,
            strings: s.strings_built,
            arena_bytes_in_use: s.arena_bytes_in_use,
            arena_bytes_reserved: s.arena_bytes_reserved,
            arena_chunks: s.arena_chunks,
            egress_bytes_in_use: s.egress_bytes_in_use,
            egress_bytes_reserved: s.egress_bytes_reserved,
            egress_high_water: s.egress_high_water,
        }
    };
    // Safety: `out` was checked non-null and writable for the struct's own size,
    // and `OmniStats` is `Copy` with no drop.
    unsafe { *out = stats };
    OMNI_OK
}

/// Copy the most recent engine error into caller-owned storage.
///
/// Writes at most `cap - 1` bytes and always NUL-terminates, so a too-small
/// buffer truncates instead of overflowing. Returns the number of bytes written
/// excluding the terminator.
///
/// # Safety
///
/// `engine` must come from [`omni_engine_new`]; `buf` must be writable for
/// `cap` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn omni_last_error(
    engine: *mut OmniEngine,
    buf: *mut c_char,
    cap: usize,
) -> usize {
    if engine.is_null() || buf.is_null() || cap == 0 {
        return 0;
    }
    // Copy the *rendered* error, not just its message. A host that shows a
    // user "expected a name" with no line number cannot point them at the
    // source, and this boundary is where that context would otherwise be lost.
    let rendered = match borrow(unsafe { &*engine }) {
        Ok(engine) => match engine.last_error() {
            Some(error) => error.to_string(),
            None => String::new(),
        },
        // Zero, which a host already reads as "there was no error". A callback
        // that wants the failure it just caused should return the message in
        // `out` rather than ask for it here.
        Err(_) => return 0,
    };
    let message = rendered.as_bytes();
    let n = message.len().min(cap - 1);
    // Safety: `buf` is writable for `cap` bytes and n < cap, so the copy and the
    // terminator at buf[n] both stay inside the caller's buffer.
    unsafe {
        ptr::copy_nonoverlapping(message.as_ptr(), buf.cast::<u8>(), n);
        *buf.add(n) = 0;
    }
    n
}

/// Test hook that proves the panic boundary is wired, from Rust or from a host.
/// There is no other way to exercise the acceptance criterion "a panic returns
/// `OMNI_ERR_PANIC`", and it is named so nobody mistakes it for an API.
///
/// It takes the handle first, because a panic started while an evaluation holds
/// the engine would unwind through the host frames that call is sitting on. From
/// inside a callback this returns [`OMNI_ERR_REENTRANT`] and does not panic at
/// all, which is the third rule doing its job on the crate's own test hook.
///
/// # Safety
///
/// `engine` must come from [`omni_engine_new`] and still be live.
#[doc(hidden)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn omni_debug_panic(engine: *mut OmniEngine) -> u32 {
    if engine.is_null() {
        return OMNI_ERR_ARG;
    }
    // Safety: the null check above ran and the caller guarantees the handle is
    // live, which is the same contract every other export in this file takes.
    let _guard = match borrow_mut(unsafe { &*engine }) {
        Ok(borrow) => borrow,
        Err(code) => return code,
    };
    guarded(|| panic!("omni_debug_panic: deliberate panic to prove the FFI boundary catches it"))
}

/// Register a function a host owns, callable from scripts as `name(...)`.
///
/// The engine stores the pointer and the `user_data` it is given and never
/// dereferences either — `user_data` only ever travels back to `callback`, so a
/// host may hang whatever it needs off it: a `PyObject *`, a JavaScript function,
/// an index into the host's own table.
///
/// Redefining `name` replaces the callback and its `user_data` together, and does
/// **not** release anything the previous registration owned. That is deliberate —
/// the engine cannot know whether the old `user_data` was freed — but it is why
/// [`omni_unregister_host_fn`] exists, and why both SDKs unregister before they
/// drop a callback.
///
/// `arity` is fixed: a script that passes a different count is a runtime error
/// naming the function and both counts, checked by the engine before the host is
/// entered.
///
/// Returns [`OMNI_OK`], [`OMNI_ERR_ARG`] for a null handle or callback, an empty
/// or over-[`MAX_IDENTIFIER_LEN`] name, an `arity` over [`MAX_ARGS`], or
/// [`OMNI_ERR_REENTRANT`] when called from inside one of this engine's own
/// callbacks.
///
/// # Safety
///
/// `engine` must come from [`omni_engine_new`]. `name` must be readable for
/// `name_len` bytes, or be null with a zero length. `callback` must be callable
/// for every argument tuple the registered `arity` describes, must not unwind,
/// and `user_data` must stay valid for every call until this name is
/// unregistered or the engine is freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn omni_register_host_fn(
    engine: *mut OmniEngine,
    name: *const c_char,
    name_len: usize,
    arity: u32,
    callback: OmniHostFn,
    user_data: *mut c_void,
) -> u32 {
    if engine.is_null() || callback.is_none() {
        return OMNI_ERR_ARG;
    }
    if name_len > MAX_IDENTIFIER_LEN || (name.is_null() && name_len != 0) {
        return OMNI_ERR_ARG;
    }
    // Safety: the length is bounded and the caller guarantees `name` is readable
    // for it.
    let name_bytes = if name_len == 0 {
        &[]
    } else {
        unsafe { slice::from_raw_parts(name.cast::<u8>(), name_len) }
    };
    let Ok(name) = std::str::from_utf8(name_bytes) else {
        return OMNI_ERR_ARG;
    };
    let mut engine = match borrow_mut(unsafe { &*engine }) {
        Ok(borrow) => borrow,
        Err(code) => return code,
    };
    // The pointer and its context are captured, not inspected: this closure is
    // the only place either is used, and it runs on the thread holding the borrow.
    let callback = callback.expect("checked non-null above");
    let context = HostContext(user_data);
    guarded(|| {
        match engine.register_host_fn(name, arity, move |engine, args| {
            call_host(engine, Some(callback), context, args)
        }) {
            Ok(()) => OMNI_OK,
            Err(HostError::EmptyName)
            | Err(HostError::NameTooLong)
            | Err(HostError::NotUtf8)
            | Err(HostError::TooLong)
            | Err(HostError::ArityTooLarge) => OMNI_ERR_ARG,
        }
    })
}

/// Drop the host function called `name`, and return 1 if there was one.
///
/// The engine releases the callback it stored; what `user_data` pointed at was
/// never its to free. A koffi slot, a ctypes trampoline, a C closure — releasing
/// those is the caller's half of the registration, and this is the call that makes
/// it safe to do them.
///
/// # Safety
///
/// As [`omni_register_host_fn`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn omni_unregister_host_fn(
    engine: *mut OmniEngine,
    name: *const c_char,
    name_len: usize,
) -> usize {
    if engine.is_null() || name_len > MAX_IDENTIFIER_LEN || (name.is_null() && name_len != 0) {
        return 0;
    }
    // Safety: bounded length, and the caller guarantees `name` is readable for it.
    let name_bytes = if name_len == 0 {
        &[]
    } else {
        unsafe { slice::from_raw_parts(name.cast::<u8>(), name_len) }
    };
    let Ok(name) = std::str::from_utf8(name_bytes) else {
        return 0;
    };
    let mut engine = match borrow_mut(unsafe { &*engine }) {
        Ok(borrow) => borrow,
        Err(_) => return 0,
    };
    guarded_len(|| usize::from(engine.unregister_host_fn(name)))
}

/// How many host functions this engine has registered.
///
/// Zero means no foreign code can be entered, which is what a leak test asserts
/// after it unregisters everything it registered.
///
/// # Safety
///
/// `engine` must come from [`omni_engine_new`], or be null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn omni_host_fn_count(engine: *const OmniEngine) -> usize {
    if engine.is_null() {
        return 0;
    }
    let engine = match borrow(unsafe { &*engine }) {
        Ok(borrow) => borrow,
        Err(_) => return 0,
    };
    guarded_len(|| engine.host_fn_count())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `omni_exec` a program and hand back the raw struct, freeing nothing: the
    /// engine stays alive because these tests read addresses out of it.
    fn run(engine: *mut OmniEngine, src: &str) -> OmniValue {
        let mut out = OmniValue::default();
        let code = unsafe { omni_exec(engine, src.as_ptr().cast::<c_char>(), src.len(), &mut out) };
        assert_eq!(code, OMNI_OK, "{src} failed");
        out
    }

    fn copy(engine: *const OmniEngine, value: &OmniValue, cap: usize) -> Vec<u8> {
        let mut buf = vec![0u8; cap];
        let n =
            unsafe { omni_str_copy(engine, value, buf.as_mut_ptr().cast::<c_char>(), buf.len()) };
        buf.truncate(n);
        buf
    }

    #[test]
    fn the_abi_reports_four_and_the_header_agrees() {
        assert_eq!(omni_abi_version(), OMNI_ABI_VERSION);
        assert_eq!(
            OMNI_ABI_VERSION, 4,
            "the header and both SDKs hard-code this"
        );
    }

    #[test]
    fn layout_is_what_the_header_claims() {
        assert_eq!(std::mem::size_of::<OmniValue>(), 16);
        assert_eq!(std::mem::offset_of!(OmniValue, tag), 0);
        assert_eq!(std::mem::offset_of!(OmniValue, len), 4);
        assert_eq!(std::mem::offset_of!(OmniValue, payload), 8);
        assert_eq!(std::mem::size_of::<OmniStats>(), 72);
        assert_eq!(std::mem::offset_of!(OmniStats, size), 0);
        assert_eq!(std::mem::offset_of!(OmniStats, last_exec_micros), 4);
        assert_eq!(std::mem::offset_of!(OmniStats, execs), 8);
    }

    #[test]
    fn scalars_encode_into_the_number_arm() {
        assert_eq!(
            encode(&Engine::new(), Value::from_f64(42.0)),
            OmniValue {
                tag: OMNI_NUMBER,
                len: 0,
                payload: OmniPayload { number: 42.0 },
            }
        );
        assert_eq!(
            encode(&Engine::new(), Value::TRUE),
            OmniValue {
                tag: OMNI_BOOL,
                len: 0,
                payload: OmniPayload { number: 1.0 },
            }
        );
        assert_eq!(encode(&Engine::new(), Value::NIL).tag, OMNI_NIL);
        assert_eq!(encode(&Engine::new(), Value::UNDEF).tag, OMNI_UNDEF);
        // A boolean crosses as a number in the payload arm, which is what lets
        // `omni_exec_number`'s tag check be the only truth test on the wire.
        assert_eq!(encode(&Engine::new(), Value::TRUE).number(), Some(1.0));
        assert_eq!(encode(&Engine::new(), Value::FALSE).number(), Some(0.0));
        assert_eq!(encode(&Engine::new(), Value::NIL).number(), None);
    }

    #[test]
    fn a_number_result_carries_no_length_and_a_string_carries_its_bytes() {
        let engine = omni_engine_new();
        let n = run(engine, "1 + 2");
        assert_eq!((n.tag, n.len), (OMNI_NUMBER, 0));
        assert_eq!(n.number(), Some(3.0));
        assert_eq!(n.string_ptr(), None);

        let s = run(engine, r#""omni""#);
        assert_eq!(s.tag, OMNI_STRING);
        assert_eq!(s.len, 4);
        assert!(
            s.number().is_none(),
            "a string must not answer to the number arm"
        );
        assert_eq!(copy(engine, &s, 16), b"omni");
        unsafe { omni_engine_free(engine) };
    }

    #[test]
    fn an_empty_string_is_a_string_not_a_null() {
        let engine = omni_engine_new();
        // `"\"\""` is the program `""`; a raw string would need four quotes and a
        // closing `#` to say the same thing, and reads as three quotes first.
        let s = run(engine, "\"\"");
        assert_eq!(
            s.tag, OMNI_STRING,
            "empty is a value, not the absence of one"
        );
        assert_eq!(s.len, 0);
        assert_eq!(
            s.string_ptr(),
            Some(ptr::null()),
            "the tag says string, the address says there is nothing to read"
        );
        assert_eq!(copy(engine, &s, 8), Vec::<u8>::new());
        unsafe { omni_engine_free(engine) };
    }

    #[test]
    fn copying_truncates_to_the_buffer_and_never_writes_a_terminator() {
        let engine = omni_engine_new();
        let s = run(engine, r#""hello, world""#);
        assert_eq!(s.len, 12);
        // `cap` six, buffer seven: the sentinel is one past what the callee may
        // touch, and it says whether the copy respected the bound.
        let mut buf = [0u8; 7];
        buf[6] = b'!';
        let n = unsafe { omni_str_copy(engine, &s, buf.as_mut_ptr().cast::<c_char>(), 6) };
        assert_eq!(n, 6, "cap bytes copied when the payload is longer");
        assert_eq!(&buf[..6], b"hello,");
        assert_eq!(buf[6], b'!', "nothing written past cap");
        assert_ne!(buf[5], 0, "and no NUL terminator inside it");
        // The exact fit needs no room for a terminator, because there is none.
        let mut exact = [0xAAu8; 12];
        let n =
            unsafe { omni_str_copy(engine, &s, exact.as_mut_ptr().cast::<c_char>(), exact.len()) };
        assert_eq!(n, 12);
        assert_eq!(&exact, b"hello, world");
        unsafe { omni_engine_free(engine) };
    }

    #[test]
    fn a_stale_string_copies_the_bytes_that_are_there_now_rather_than_faulting() {
        let engine = omni_engine_new();
        let first = run(engine, r#""first""#);
        assert_eq!(copy(engine, &first, 16), b"first");
        // The documented rule: valid until the next exec on this engine. Rewind
        // does not unmap, so the address is still owned — it now answers with what
        // the second execution wrote there, which is a wrong read, not a crash.
        let second = run(engine, r#""a much longer second""#);
        let stale = copy(engine, &first, 32);
        assert_eq!(stale, copy(engine, &second, 32), "the same bytes, honestly");
        assert_eq!(stale, b"a much longer second");
        unsafe { omni_engine_free(engine) };
    }

    #[test]
    fn a_fabricated_address_and_another_engines_address_both_copy_zero() {
        let engine = omni_engine_new();
        let mine = run(engine, r#""mine""#);
        let other = omni_engine_new();
        let theirs = run(other, r#""theirs""#);

        // Another engine's address, read through the wrong handle.
        assert_eq!(
            copy(engine, &theirs, 16),
            Vec::<u8>::new(),
            "a pointer is not a proof of ownership"
        );
        assert_eq!(
            copy(other, &theirs, 16),
            b"theirs",
            "and its own engine reads it"
        );

        // An address out of a stack variable.
        let local: u64 = 0xDEAD_BEEF;
        let fake = OmniValue {
            tag: OMNI_STRING,
            len: 4,
            payload: OmniPayload {
                ptr: std::ptr::addr_of!(local).cast::<c_char>(),
            },
        };
        assert_eq!(copy(engine, &fake, 16), Vec::<u8>::new());

        // An address the 48-bit format cannot hold at all.
        let high = OmniValue {
            tag: OMNI_STRING,
            len: 4,
            payload: OmniPayload {
                ptr: ptr::without_provenance(1 << 48),
            },
        };
        assert_eq!(copy(engine, &high, 16), Vec::<u8>::new());

        // Null and zero-cap are the same answer without a dereference.
        assert_eq!(
            unsafe { omni_str_copy(engine, &mine, ptr::null_mut(), 16,) },
            0
        );
        let mut buf = [0u8; 16];
        assert_eq!(
            unsafe { omni_str_copy(engine, ptr::null(), buf.as_mut_ptr().cast::<c_char>(), 16) },
            0
        );
        assert_eq!(
            unsafe { omni_str_copy(engine, &mine, buf.as_mut_ptr().cast::<c_char>(), 0) },
            0
        );
        assert_eq!(
            unsafe { omni_str_copy(ptr::null(), &mine, buf.as_mut_ptr().cast::<c_char>(), 16) },
            0
        );
        unsafe {
            omni_engine_free(engine);
            omni_engine_free(other);
        };
    }

    #[test]
    fn a_number_value_copies_nothing_even_with_a_plausible_pointer() {
        let engine = omni_engine_new();
        let n = run(engine, "7");
        let mut buf = [0u8; 8];
        let written =
            unsafe { omni_str_copy(engine, &n, buf.as_mut_ptr().cast::<c_char>(), buf.len()) };
        assert_eq!(written, 0, "the tag is the first gate");
        assert_eq!(buf, [0u8; 8]);
        unsafe { omni_engine_free(engine) };
    }

    #[test]
    fn define_string_binds_bytes_the_host_can_then_free() {
        let engine = omni_engine_new();
        let name = b"greeting";
        let payload = String::from("নম্বর ✄").into_bytes();
        let code = unsafe {
            omni_define_string(
                engine,
                name.as_ptr().cast::<c_char>(),
                name.len(),
                payload.as_ptr().cast::<c_char>(),
                payload.len(),
            )
        };
        assert_eq!(code, OMNI_OK);
        drop(payload);
        // The program reads the binding, and the result copies out of the engine's
        // own storage rather than the caller's, which is already gone.
        let s = run(engine, "greeting");
        assert_eq!(s.tag, OMNI_STRING);
        assert_eq!(copy(engine, &s, 64), String::from("নম্বর ✄").into_bytes());
        unsafe { omni_engine_free(engine) };
    }

    #[test]
    fn define_string_rejects_every_bad_argument_before_touching_memory() {
        let engine = omni_engine_new();
        let ok = b"n";
        let reject = |name: &[u8], bytes: &[u8]| {
            let code = unsafe {
                omni_define_string(
                    engine,
                    name.as_ptr().cast::<c_char>(),
                    name.len(),
                    bytes.as_ptr().cast::<c_char>(),
                    bytes.len(),
                )
            };
            assert_eq!(
                code,
                OMNI_ERR_ARG,
                "{} / {}",
                String::from_utf8_lossy(name),
                bytes.len()
            );
        };
        reject(b"", b"x");
        reject(&[0xFF], b"x");
        reject(b"n", &[0xFF]);
        reject(&[b'a'; MAX_IDENTIFIER_LEN + 1], b"x");
        // A null buffer with a non-zero length is an argument error, not a read.
        let code = unsafe {
            omni_define_string(
                engine,
                ok.as_ptr().cast::<c_char>(),
                ok.len(),
                ptr::null(),
                4,
            )
        };
        assert_eq!(code, OMNI_ERR_ARG);
        // A length over the ceiling is rejected without fabricating a slice.
        let code = unsafe {
            omni_define_string(
                engine,
                ok.as_ptr().cast::<c_char>(),
                ok.len(),
                b"x".as_ptr().cast::<c_char>(),
                MAX_STRING_LEN + 1,
            )
        };
        assert_eq!(code, OMNI_ERR_ARG);
        // An empty payload is a legitimate binding, so it must not be rejected.
        let code = unsafe {
            omni_define_string(
                engine,
                ok.as_ptr().cast::<c_char>(),
                ok.len(),
                b"".as_ptr().cast::<c_char>(),
                0,
            )
        };
        assert_eq!(code, OMNI_OK);
        unsafe { omni_engine_free(engine) };
    }

    #[test]
    fn redefining_a_name_makes_the_old_value_copy_zero() {
        let engine = omni_engine_new();
        let name = b"k";
        let first = b"first".to_vec();
        let second = b"second".to_vec();
        unsafe {
            omni_define_string(
                engine,
                name.as_ptr().cast::<c_char>(),
                name.len(),
                first.as_ptr().cast::<c_char>(),
                first.len(),
            );
        }
        let old = run(engine, "k");
        assert_eq!(copy(engine, &old, 16), b"first");
        unsafe {
            omni_define_string(
                engine,
                name.as_ptr().cast::<c_char>(),
                name.len(),
                second.as_ptr().cast::<c_char>(),
                second.len(),
            );
        }
        assert_eq!(
            copy(engine, &old, 16),
            Vec::<u8>::new(),
            "the superseded bytes are freed, so the ownership check is what stops a read"
        );
        let now = run(engine, "k");
        assert_eq!(copy(engine, &now, 16), b"second");
        unsafe { omni_engine_free(engine) };
    }

    #[test]
    fn stats_reports_the_counters_and_their_size() {
        let engine = omni_engine_new();
        let mut before = OmniStats::default();
        assert_eq!(unsafe { omni_debug_stats(engine, &mut before) }, OMNI_OK);
        assert_eq!(before.size, std::mem::size_of::<OmniStats>() as u32);
        assert_eq!(before.execs, 0);
        assert_eq!(before.last_exec_micros, 0, "nothing has run yet");
        for _ in 0..50 {
            let _ = run(engine, r#""a" + "b""#);
        }
        let mut after = OmniStats::default();
        assert_eq!(unsafe { omni_debug_stats(engine, &mut after) }, OMNI_OK);
        assert_eq!(after.execs, 50);
        assert!(after.strings > before.strings);
        assert!(after.egress_bytes_in_use > 0, "a result lives in egress");
        // Layout mismatch detection, from the host's side.
        let mut wrong = OmniStats {
            size: 1,
            ..OmniStats::default()
        };
        assert_eq!(unsafe { omni_debug_stats(engine, &mut wrong) }, OMNI_OK);
        assert_ne!(wrong.size, 1, "the callee always restates the size");
        assert_eq!(
            unsafe { omni_debug_stats(ptr::null_mut(), &mut after) },
            OMNI_ERR_ARG
        );
        assert_eq!(
            unsafe { omni_debug_stats(engine, ptr::null_mut()) },
            OMNI_ERR_ARG
        );
        unsafe { omni_engine_free(engine) };
    }

    #[test]
    fn the_timer_crosses_the_boundary_in_the_four_padding_bytes() {
        let engine = omni_engine_new();
        let mut stats = OmniStats::default();
        assert_eq!(unsafe { omni_debug_stats(engine, &mut stats) }, OMNI_OK);
        assert_eq!(stats.last_exec_micros, 0, "nothing has run yet");
        // A literal large enough that lexing it cannot round down to zero.
        let src = format!("\"{}\"", "x".repeat(200_000));
        let _ = run(engine, &src);
        assert_eq!(unsafe { omni_debug_stats(engine, &mut stats) }, OMNI_OK);
        assert!(stats.last_exec_micros > 0, "the timer did not move");
        // The field moved into padding, so the size a v4 host checks is unchanged.
        assert_eq!(stats.size, 72);
        unsafe { omni_engine_free(engine) };
    }

    #[test]
    fn exec_number_still_refuses_a_string() {
        let engine = omni_engine_new();
        let mut n = 0.0;
        assert_eq!(
            unsafe { omni_exec_number(engine, r#""hi""#.as_ptr().cast::<c_char>(), 4, &mut n,) },
            OMNI_ERR_RUNTIME
        );
        assert_eq!(
            unsafe { omni_exec_number(engine, "1 + 1".as_ptr().cast::<c_char>(), 5, &mut n) },
            OMNI_OK
        );
        assert_eq!(n, 2.0);
        unsafe { omni_engine_free(engine) };
    }

    #[test]
    fn every_error_kind_maps_to_a_distinct_status() {
        assert_eq!(status_for(ErrorKind::Lex), OMNI_ERR_COMPILE);
        assert_eq!(status_for(ErrorKind::Parse), OMNI_ERR_COMPILE);
        assert_eq!(status_for(ErrorKind::Runtime), OMNI_ERR_RUNTIME);
        assert_eq!(status_for(ErrorKind::Panic), OMNI_ERR_PANIC);
    }

    #[test]
    fn a_type_error_and_a_bad_escape_are_compile_errors_not_argument_errors() {
        let engine = omni_engine_new();
        let mut out = OmniValue::default();
        let mixing = r#""a" + 1"#;
        let code = unsafe {
            omni_exec(
                engine,
                mixing.as_ptr().cast::<c_char>(),
                mixing.len(),
                &mut out,
            )
        };
        assert_eq!(code, OMNI_ERR_RUNTIME);
        let mut buf = [0i8; 128];
        let n = unsafe { omni_last_error(engine, buf.as_mut_ptr().cast::<c_char>(), buf.len()) };
        let text =
            String::from_utf8_lossy(unsafe { slice::from_raw_parts(buf.as_ptr().cast::<u8>(), n) })
                .to_string();
        assert!(
            text.contains(r#"cannot add string to number: "a" + 1"#),
            "the message the requester asked for, across the boundary: {text}"
        );
        // An illegal escape is a program error, so it reports as a compile error.
        let bad_escape = r#""\q""#;
        let code = unsafe {
            omni_exec(
                engine,
                bad_escape.as_ptr().cast::<c_char>(),
                bad_escape.len(),
                &mut out,
            )
        };
        assert_eq!(code, OMNI_ERR_COMPILE);
        unsafe { omni_engine_free(engine) };
    }

    #[test]
    fn oversized_len_is_rejected_before_any_slice_is_fabricated() {
        let engine = omni_engine_new();
        let mut out = OmniValue::default();
        let code = unsafe {
            omni_exec(
                engine,
                "1".as_ptr().cast::<c_char>(),
                MAX_SOURCE_LEN + 1,
                &mut out,
            )
        };
        assert_eq!(code, OMNI_ERR_ARG);
        unsafe { omni_engine_free(engine) };
    }

    #[test]
    fn the_panic_boundary_still_covers_the_new_bodies() {
        let engine = omni_engine_new();
        assert_eq!(unsafe { omni_debug_panic(engine) }, OMNI_ERR_PANIC);
        // The other four new bodies are guarded in the same shape, and a host
        // reaching them with an address the engine does not own must get a clean
        // zero rather than a partial write.
        let foreign = OmniValue {
            tag: OMNI_STRING,
            len: u32::MAX,
            payload: OmniPayload {
                ptr: ptr::without_provenance(8),
            },
        };
        let mut buf = [0u8; 16];
        let n = unsafe { omni_str_copy(engine, &foreign, buf.as_mut_ptr().cast::<c_char>(), 16) };
        assert_eq!(n, 0, "an address the engine does not own copies nothing");
        assert_eq!(buf, [0u8; 16], "and writes nothing on the way out");
        unsafe { omni_engine_free(engine) };
    }

    fn check_manifest(manifest: &str) -> (i32, String) {
        let mut buf = [0i8; 64];
        let code = unsafe {
            omni_check_update(
                manifest.as_ptr().cast::<c_char>(),
                manifest.len(),
                buf.as_mut_ptr(),
                buf.len(),
            )
        };
        let text = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        (code, text)
    }

    #[test]
    fn the_update_export_compares_against_this_build() {
        let current = env!("CARGO_PKG_VERSION");
        let same = format!(r#"{{"version":"{current}","abi":{OMNI_ABI_VERSION}}}"#);
        assert_eq!(
            check_manifest(&same),
            (OMNI_UPDATE_UP_TO_DATE, current.to_string())
        );

        let newer = format!(r#"{{"version":"99.0.0","abi":{OMNI_ABI_VERSION}}}"#);
        assert_eq!(
            check_manifest(&newer),
            (OMNI_UPDATE_AVAILABLE, "99.0.0".into())
        );

        // Another wire format is refused before the numbers are compared.
        let foreign = format!(r#"{{"version":"99.0.0","abi":{}}}"#, OMNI_ABI_VERSION + 1);
        assert_eq!(check_manifest(&foreign).0, OMNI_UPDATE_ABI_MISMATCH);

        // Older and malformed manifests are distinguishable answers, not errors.
        let older = format!(r#"{{"version":"0.0.1","abi":{OMNI_ABI_VERSION}}}"#);
        assert_eq!(check_manifest(&older).0, OMNI_UPDATE_UP_TO_DATE);
        assert_eq!(check_manifest("{").0, OMNI_UPDATE_INVALID);
        assert_eq!(check_manifest("").0, OMNI_UPDATE_INVALID);
    }

    #[test]
    fn the_update_export_truncates_and_rejects_bad_arguments() {
        let manifest = format!(r#"{{"version":"99.0.0","abi":{OMNI_ABI_VERSION}}}"#);
        let mut buf = [0i8; 3];
        let code = unsafe {
            omni_check_update(
                manifest.as_ptr().cast::<c_char>(),
                manifest.len(),
                buf.as_mut_ptr(),
                buf.len(),
            )
        };
        assert_eq!(code, OMNI_UPDATE_AVAILABLE);
        // Two bytes and a terminator: a short buffer is a truncated answer, not
        // a memory error.
        assert_eq!(
            unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }.to_bytes(),
            b"99"
        );

        // A null destination still answers the question.
        let code = unsafe {
            omni_check_update(
                manifest.as_ptr().cast::<c_char>(),
                manifest.len(),
                ptr::null_mut(),
                0,
            )
        };
        assert_eq!(code, OMNI_UPDATE_AVAILABLE);

        // A null manifest with a length, non-UTF-8 bytes, and an over-long
        // document are argument errors rather than reads.
        assert_eq!(
            unsafe { omni_check_update(ptr::null(), 4, ptr::null_mut(), 0) },
            OMNI_UPDATE_BAD_ARG
        );
        let bad = [0xFFu8, 0xFE];
        assert_eq!(
            unsafe {
                omni_check_update(bad.as_ptr().cast::<c_char>(), bad.len(), ptr::null_mut(), 0)
            },
            OMNI_UPDATE_BAD_ARG
        );
        assert_eq!(
            unsafe {
                omni_check_update(
                    manifest.as_ptr().cast::<c_char>(),
                    omni_core::update::MAX_MANIFEST_LEN + 1,
                    ptr::null_mut(),
                    0,
                )
            },
            OMNI_UPDATE_BAD_ARG
        );
    }
}
