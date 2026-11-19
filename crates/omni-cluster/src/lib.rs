//! OmniEngine cluster execution: a std-only worker protocol.
//!
//! A [`WorkerNode`] listens on TCP and executes sandboxed OmniEngine tasks
//! dispatched by a [`ClusterClient`]. The wire channel is authenticated and
//! encrypted with a pre-shared key via the AEAD primitives in [`crypto`],
//! which implement the TLS 1.3 PSK record-layer key schedule (extract/expand,
//! per-direction keys, AEAD sealing, sequence-number replay rejection). This
//! is *not* TLS: on untrusted networks front workers with a TLS-terminating
//! proxy, and treat the PSK channel as the authentication layer inside it.
//!
//! Every remote task runs on `Engine::fork` of a template engine, with a gas
//! ceiling enforced via `set_step_limit` and a fail-closed permission set
//! fixed at node startup. Remote tasks cannot widen permissions, cannot
//! import modules, and cannot escape the step budget the node operator chose.

pub mod client;
pub mod crypto;
pub mod node;
pub mod wire;

pub use client::{ClusterClient, RemoteError, TaskReply};
pub use node::{WorkerConfig, WorkerNode};
pub use wire::{ErrorCode, TaskStatus, ValueTag, MAX_CLUSTER_TASK};




/// OS random bytes for wire nonces. Unix reads `/dev/urandom`; Windows calls
/// `BCryptGenRandom` through a fixed import table (the system bcrypt DLL is
/// always present). There is no userspace fallback: a node that cannot get
/// OS randomness must not come up, because nonce reuse would defeat the
/// channel's replay protection.
pub(crate) fn os_random(buf: &mut [u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Read;
        std::fs::File::open("/dev/urandom")?.read_exact(buf)
    }
    #[cfg(windows)]
    {
        windows_random(buf)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = buf;
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "os_random: unsupported platform",
        ))
    }
}

#[cfg(windows)]
fn windows_random(buf: &mut [u8]) -> std::io::Result<()> {
    // BCRYPT_USE_SYSTEM_PREFERRED_RNG = 0x00000002; the algorithm handle may
    // be null when that flag is used.
    #[link(name = "bcrypt")]
    unsafe extern "system" {
        fn BCryptGenRandom(
            h_algorithm: *mut core::ffi::c_void,
            pb_buffer: *mut u8,
            cb_buffer: u32,
            dw_flags: u32,
        ) -> i32;
    }
    // Safety: `buf` is a live writable slice for its full length; the call
    // writes cb_buffer bytes before returning.
    let status = unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            buf.as_mut_ptr(),
            buf.len() as u32,
            0x00000002,
        )
    };
    if status == 0 {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("BCryptGenRandom failed: NTSTATUS 0x{status:08x}"),
        ))
    }
}
