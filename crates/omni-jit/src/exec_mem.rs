//! One executable allocation per platform: `VirtualAlloc` on Windows,
//! `mmap` everywhere else. Hand-rolled rather than pulled from a JIT-linker
//! crate because a leaf function has nothing to link, and the tier's
//! zero-relocation contract is worth keeping visible.
//!
//! The allocation is writable while the code is copied in and executable
//! afterwards; the instruction cache is flushed on the one architecture that
//! does not keep it coherent with data writes (aarch64).

#[cfg(windows)]
mod platform {
    use std::ffi::c_void;

    const MEM_COMMIT_RESERVE: u32 = 0x3000;
    const PAGE_EXECUTE_READWRITE: u32 = 0x40;
    const MEM_RELEASE: u32 = 0x8000;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn VirtualAlloc(
            address: *mut c_void,
            size: usize,
            allocation_type: u32,
            protect: u32,
        ) -> *mut c_void;
        fn VirtualFree(address: *mut c_void, size: usize, free_type: u32) -> i32;
        fn FlushInstructionCache(process: *mut c_void, base: *const c_void, size: usize) -> i32;
        fn GetCurrentProcess() -> *mut c_void;
    }

    pub struct ExecMem {
        base: *mut u8,
        // MEM_RELEASE takes a zero size, so Windows has no use for it; the
        // unix Drop needs the length for munmap.
        #[cfg(unix)]
        len: usize,
    }

    // The region belongs exclusively to this handle, and the bytes it holds
    // are immutable once written, so calling the code from another thread is
    // sound.
    unsafe impl Send for ExecMem {}

    impl ExecMem {
        pub fn new(code: &[u8]) -> Option<ExecMem> {
            if code.is_empty() {
                return None;
            }
            // Safety: the region is committed for exactly `code.len()` bytes
            // and the copy lands inside it. `FlushInstructionCache` is called
            // while the region is writable, as the API requires.
            unsafe {
                let base = VirtualAlloc(
                    std::ptr::null_mut(),
                    code.len(),
                    MEM_COMMIT_RESERVE,
                    PAGE_EXECUTE_READWRITE,
                );
                if base.is_null() {
                    return None;
                }
                std::ptr::copy_nonoverlapping(code.as_ptr(), base.cast::<u8>(), code.len());
                FlushInstructionCache(GetCurrentProcess(), base, code.len());
                Some(ExecMem { base: base.cast() })
            }
        }

        pub fn as_ptr(&self) -> *const u8 {
            self.base
        }
    }

    impl Drop for ExecMem {
        fn drop(&mut self) {
            // Safety: `base` came from the `VirtualAlloc` in `new`; a zero
            // size with MEM_RELEASE frees the whole region.
            unsafe {
                VirtualFree(self.base.cast(), 0, MEM_RELEASE);
            }
        }
    }
}

#[cfg(unix)]
mod platform {
    use std::ffi::{c_char, c_void};

    const PROT_READ_WRITE_EXEC: i32 = 1 | 2 | 4;
    const MAP_PRIVATE: i32 = 2;
    #[cfg(target_os = "macos")]
    const MAP_ANONYMOUS: i32 = 0x1000;
    #[cfg(not(target_os = "macos"))]
    const MAP_ANONYMOUS: i32 = 0x20;

    unsafe extern "C" {
        fn mmap(
            address: *mut c_void,
            length: usize,
            prot: i32,
            flags: i32,
            fd: i32,
            offset: i64,
        ) -> *mut c_void;
        fn munmap(address: *mut c_void, length: usize) -> i32;
    }

    #[cfg(all(target_arch = "aarch64", target_os = "linux"))]
    unsafe extern "C" {
        fn __clear_cache(start: *mut c_char, end: *mut c_char);
    }
    #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
    unsafe extern "C" {
        fn sys_icache_invalidate(start: *mut c_void, length: usize);
    }

    pub struct ExecMem {
        base: *mut u8,
        len: usize,
    }

    // See the Windows twin for why this is sound.
    unsafe impl Send for ExecMem {}

    impl ExecMem {
        pub fn new(code: &[u8]) -> Option<ExecMem> {
            if code.is_empty() {
                return None;
            }
            // Safety: MAP_ANONYMOUS ignores `fd` and `offset`; the mapping is
            // private and sized exactly to the copy.
            unsafe {
                let base = mmap(
                    std::ptr::null_mut(),
                    code.len(),
                    PROT_READ_WRITE_EXEC,
                    MAP_PRIVATE | MAP_ANONYMOUS,
                    -1,
                    0,
                );
                if base == usize::MAX as *mut c_void {
                    return None;
                }
                std::ptr::copy_nonoverlapping(code.as_ptr(), base.cast::<u8>(), code.len());
                flush_instruction_cache(base, code.len());
                Some(ExecMem {
                    base: base.cast(),
                    len: code.len(),
                })
            }
        }

        pub fn as_ptr(&self) -> *const u8 {
            self.base
        }
    }

    impl Drop for ExecMem {
        fn drop(&mut self) {
            // Safety: `base` came from the `mmap` in `new`, with `len` bytes.
            unsafe {
                munmap(self.base.cast(), self.len);
            }
        }
    }

    /// x86-64 keeps the data and instruction caches coherent; aarch64 does
    /// not, so freshly written code must be flushed before the first call.
    #[cfg(target_arch = "aarch64")]
    unsafe fn flush_instruction_cache(base: *mut c_void, len: usize) {
        #[cfg(target_os = "linux")]
        // Safety: both pointers bound the mapping written in `new`.
        unsafe {
            __clear_cache(base.cast::<c_char>(), (base as usize + len) as *mut c_char);
        }
        #[cfg(target_os = "macos")]
        // Safety: the range lies inside the mapping written in `new`.
        unsafe {
            sys_icache_invalidate(base, len);
        }
    }

    #[cfg(not(target_arch = "aarch64"))]
    unsafe fn flush_instruction_cache(_base: *mut c_void, _len: usize) {}
}

pub use platform::ExecMem;
