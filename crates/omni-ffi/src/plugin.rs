//! Native plugin loading: a hand-rolled `dlopen`/`LoadLibrary`, strict symbol
//! verification, and registration into the host-function table.
//!
//! A plugin is a shared library (`omni_plugin_*.dll` on Windows, `.so`
//! elsewhere) exporting one symbol, `omni_plugin_manifest`, whose bytes are an
//! [`OmniPluginManifest`]. The manifest describes up to
//! [`OMNI_MAX_PLUGIN_FUNCTIONS`] functions; each is a C-unwind function the
//! engine can call by name like any other host function.
//!
//! ## Why the loader is hand-rolled
//!
//! `libloading` solves reloading, symbol versioning, and a pile of portability
//! this crate does not need. What this crate needs is three functions and a
//! strict read: `LoadLibraryW`/`dlopen`, `GetProcAddress`/`dlsym`, and
//! `FreeLibrary`/`dlclose`. Hand-rolling keeps omni-ffi's zero-dependency core
//! intact and makes the trust boundary explicit — everything below is the
//! boundary.
//!
//! ## The verification contract
//!
//! Every byte read from the plugin is read through [`std::ptr::read_unaligned`]
//! at an offset the manifest's own layout fixes, inside a fixed window around
//! the symbol, and every string is bounded and copied into owned memory before
//! the library can be unloaded. A library
//! that fails any check is refused wholesale: wrong magic, wrong size, a
//! plugin-ABI or engine-ABI mismatch, a bad name, more functions than the cap,
//! a null function pointer, an arity above [`crate::MAX_ARGS`], or a duplicate
//! name. Verification happens once, at load, so every later call through the
//! engine can trust what it calls.
//!
//! ## The panic boundary
//!
//! A plugin is compiled Rust with the same rustc, and `extern "C-unwind"`
//! exists so its panics can cross a plugin boundary *to a place that catches
//! them*. That place is [`call_plugin`]: a plugin panic becomes a host-call
//! failure the program can read, never an unwind into `omni_exec`'s foreign
//! frames, and the library stays mapped throughout.
//!
//! One measured caveat: a plugin that statically links std owns its own
//! panic runtime, and on Windows an unwind that leaves such a DLL belongs to
//! a foreign runtime — the host's `catch_unwind` cannot catch it (the
//! fixture suite proved this the hard way: the process aborts). The
//! contract therefore asks every plugin to catch its own panics at its
//! exported functions and report the message as a host failure, exactly as
//! the fixture's `crash` demonstrates. The trampoline below stays: it is the
//! last line of defense for plugins that share the host's runtime, and it is
//! what turns the panic into an error rather than a crash.

use std::any::Any;
use std::ffi::{c_char, c_int, c_void};
use std::fmt;
use std::mem::size_of;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;

use omni_core::{Engine, HostError, HostOutcome, Value};

use crate::{OMNI_ABI_VERSION, OmniValue, encode_argument};

/// The magic every manifest begins with. `0x4F4D4E49` — "OMNI" as little-endian
/// bytes.
pub const OMNI_PLUGIN_MAGIC: u32 = 0x4F4D_4E49;
/// The plugin-manifest wire version. Bumped when the manifest layout changes.
pub const OMNI_PLUGIN_ABI_VERSION: u32 = 1;
/// How many functions one manifest may declare.
pub const OMNI_MAX_PLUGIN_FUNCTIONS: usize = 64;
/// The manifest's `name` field is this many bytes, NUL-padded.
pub const OMNI_MAX_PLUGIN_NAME: usize = 32;
/// The most bytes a function name may occupy in plugin memory. Names longer
/// than this cannot pass [`crate::MAX_IDENTIFIER_LEN`] anyway; the bound
/// exists so verification reads a hostile library's string for a fixed,
/// short distance and never further.
pub const OMNI_MAX_PLUGIN_FN_NAME: usize = 64;
/// The longest path [`omni_engine_load_plugin`](crate::omni_engine_load_plugin)
/// will accept. A host that can name a longer one is not one this ABI needs
/// to serve, and the bound keeps the path read short.
pub const OMNI_MAX_PLUGIN_PATH: usize = 4096;
/// How far around the `omni_plugin_manifest` symbol the verifier will read:
/// the manifest itself, the function table it points at, and every function
/// name must all live within this many bytes *below or above* the symbol.
/// A statically declared plugin keeps all of that adjacent to the symbol
/// (linkers may place the table below it); the bound exists so a hostile
/// library can never make verification walk arbitrary memory.
const OMNI_PLUGIN_VERIFY_WINDOW: usize = 64 * 1024;

/// One exported function, as the plugin declares it.
///
/// `call` is `Option` so a null function pointer in plugin memory is `None` in
/// the type and rejected by verification instead of stored to be called.
#[repr(C)]
pub struct OmniPluginFn {
    /// NUL-terminated UTF-8 name, read at load and copied.
    pub name: *const c_char,
    /// The parameter count the engine will call it with. At most
    /// [`crate::MAX_ARGS`].
    pub arity: u32,
    /// Reserved; must be zero.
    pub reserved: u32,
    /// The function itself. `C-unwind` because the plugin and the engine share
    /// a rustc, and the engine's trampoline catches what unwinds.
    pub call: Option<PluginCall>,
}

/// The shape of a plugin function: the same contract as
/// [`crate::OmniHostFn`] with `extern "C-unwind"` instead of `extern "C"`.
///
/// `args` points at `argc` [`OmniValue`]s whose string arms address the
/// engine's own memory, valid until this function returns. Return
/// [`crate::OMNI_HOST_OK`] with the result in `out`, or
/// [`crate::OMNI_HOST_ERR`] with the message in `out`.
pub type PluginCall = unsafe extern "C-unwind" fn(
    user_data: *mut c_void,
    args: *const OmniValue,
    argc: u32,
    out: *mut OmniValue,
) -> i32;

/// The bytes behind the `omni_plugin_manifest` symbol. 80 bytes, fixed.
#[repr(C)]
pub struct OmniPluginManifest {
    pub magic: u32,
    pub size: u32,
    pub abi_version: u32,
    pub omni_abi: u32,
    pub name: [c_char; OMNI_MAX_PLUGIN_NAME],
    pub version_major: u32,
    pub version_minor: u32,
    pub version_patch: u32,
    pub reserved: u32,
    pub function_count: u32,
    pub functions: *const OmniPluginFn,
}

/// Why a plugin was refused. Every variant is the plugin's fault; the FFI maps
/// them all onto [`crate::OMNI_ERR_ARG`] and keeps the detail here, where a
/// Rust host can read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginError {
    /// An empty path, or one longer than [`OMNI_MAX_PLUGIN_PATH`].
    BadPath,
    /// The OS could not load the library at all.
    OpenFailed,
    /// The library exports no `omni_plugin_manifest` symbol.
    MissingManifest,
    /// The verifier needed bytes past the end of the readable window.
    Truncated,
    /// The manifest's magic is not [`OMNI_PLUGIN_MAGIC`].
    BadMagic(u32),
    /// The manifest's `size` is not the size of [`OmniPluginManifest`].
    BadSize(u32),
    /// The plugin was built against a different plugin-ABI version.
    BadAbiVersion(u32),
    /// The plugin was built against a different engine ABI.
    AbiMismatch { plugin: u32, engine: u32 },
    /// The manifest's name is empty.
    Unnamed,
    /// The manifest's name is not UTF-8.
    BadName,
    /// More functions than [`OMNI_MAX_PLUGIN_FUNCTIONS`] declared.
    TooManyFunctions(u32),
    /// `function_count` is nonzero but the function table is null.
    BadFunctionTable,
    /// A function's name is missing, not UTF-8, or too long.
    BadFunctionName { index: usize },
    /// A function's pointer is null.
    NullFunction { index: usize, name: String },
    /// A function declared more parameters than the ABI carries.
    ArityTooLarge { name: String, arity: u32 },
    /// Two functions declared the same name.
    DuplicateName(String),
    /// The engine refused the registration; earlier functions were rolled back.
    RegisterFailed { name: String, error: HostError },
}

impl fmt::Display for PluginError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PluginError::BadPath => f.write_str("the plugin path is empty or too long"),
            PluginError::OpenFailed => {
                f.write_str("the operating system could not load the library")
            }
            PluginError::MissingManifest => {
                f.write_str("the library exports no omni_plugin_manifest symbol")
            }
            PluginError::Truncated => {
                f.write_str("the plugin's data ended before the verifier finished reading it")
            }
            PluginError::BadMagic(magic) => write!(
                f,
                "the plugin manifest has magic {magic:#010X}, expected {:#010X}",
                OMNI_PLUGIN_MAGIC
            ),
            PluginError::BadSize(size) => write!(
                f,
                "the plugin manifest is {size} bytes, expected {}",
                size_of::<OmniPluginManifest>()
            ),
            PluginError::BadAbiVersion(version) => write!(
                f,
                "the plugin speaks plugin ABI {version}, expected {OMNI_PLUGIN_ABI_VERSION}"
            ),
            PluginError::AbiMismatch { plugin, engine } => write!(
                f,
                "the plugin speaks engine ABI {plugin}, expected {engine}"
            ),
            PluginError::Unnamed => f.write_str("the plugin manifest has an empty name"),
            PluginError::BadName => f.write_str("the plugin manifest name is not UTF-8"),
            PluginError::TooManyFunctions(count) => write!(
                f,
                "the plugin declares {count} functions, the cap is {OMNI_MAX_PLUGIN_FUNCTIONS}"
            ),
            PluginError::BadFunctionTable => {
                f.write_str("the plugin declares functions but its table is null")
            }
            PluginError::BadFunctionName { index } => write!(
                f,
                "plugin function {index} has a missing, invalid, or over-long name"
            ),
            PluginError::NullFunction { index, name } => {
                write!(f, "plugin function {index} ({name:?}) is a null pointer")
            }
            PluginError::ArityTooLarge { name, arity } => write!(
                f,
                "plugin function {name:?} declares arity {arity}, the cap is {}",
                crate::MAX_ARGS
            ),
            PluginError::DuplicateName(name) => {
                write!(f, "the plugin declares {name:?} twice")
            }
            PluginError::RegisterFailed { name, error } => {
                write!(f, "registering plugin function {name:?} failed: {error}")
            }
        }
    }
}

/// A loaded shared library. One platform pair of imports behind a uniform
/// three-method surface; dropping it unloads the module.
struct Library {
    handle: *mut c_void,
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn LoadLibraryW(name: *const u16) -> *mut c_void;
    fn GetProcAddress(handle: *mut c_void, name: *const c_char) -> *mut c_void;
    fn FreeLibrary(handle: *mut c_void) -> c_int;
}

#[cfg(unix)]
#[link(name = "dl")]
unsafe extern "C" {
    fn dlopen(name: *const c_char, flags: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void;
    fn dlclose(handle: *mut c_void) -> c_int;
}

#[cfg(unix)]
const RTLD_NOW: c_int = 2;

impl Library {
    fn open(path: &str) -> Result<Library, PluginError> {
        #[cfg(windows)]
        {
            let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
            // Safety: `wide` is NUL-terminated and outlives the call.
            let handle = unsafe { LoadLibraryW(wide.as_ptr()) };
            if handle.is_null() {
                return Err(PluginError::OpenFailed);
            }
            Ok(Library { handle })
        }
        #[cfg(unix)]
        {
            let Ok(cpath) = std::ffi::CString::new(path) else {
                return Err(PluginError::OpenFailed);
            };
            // Safety: `cpath` outlives the call. RTLD_NOW binds every symbol
            // up front, so a missing symbol is a load error and not a lazy
            // surprise at first call.
            let handle = unsafe { dlopen(cpath.as_ptr(), RTLD_NOW) };
            if handle.is_null() {
                return Err(PluginError::OpenFailed);
            }
            Ok(Library { handle })
        }
    }

    /// The address of one exported symbol, or null. `name` is NUL-terminated
    /// on every platform this function serves.
    fn symbol(&self, name: &[u8]) -> *mut c_void {
        #[cfg(windows)]
        {
            // Safety: the library is still mapped (this method borrows it),
            // and `name` is NUL-terminated.
            unsafe { GetProcAddress(self.handle, name.as_ptr().cast::<c_char>()) }
        }
        #[cfg(unix)]
        {
            // Safety: as above.
            unsafe { dlsym(self.handle, name.as_ptr().cast::<c_char>()) }
        }
    }
}

impl Drop for Library {
    fn drop(&mut self) {
        #[cfg(windows)]
        {
            // Safety: `handle` came from LoadLibraryW and has not been freed.
            unsafe {
                FreeLibrary(self.handle);
            }
        }
        #[cfg(unix)]
        {
            // Safety: `handle` came from dlopen and has not been closed.
            unsafe {
                dlclose(self.handle);
            }
        }
    }
}

/// One verified function: everything the engine needs, owned and plugin-free.
struct PluginFunction {
    name: String,
    arity: u32,
    call: PluginCall,
}

/// The plugin's own metadata, copied out of its memory during verification.
struct Verified {
    name: String,
    functions: Vec<PluginFunction>,
}

/// A loaded, verified plugin. Keeping it alive keeps the library mapped; the
/// engine's handle owns one of these per successful load and drops it only
/// after the engine itself.
pub struct Plugin {
    _lib: Library,
    name: String,
    functions: Vec<PluginFunction>,
}

impl Plugin {
    /// Load `path`, resolve and verify the manifest, and copy every function's
    /// metadata into owned memory. Nothing here retains a pointer into the
    /// plugin except through `_lib`, which keeps the module mapped.
    pub fn load(path: &str) -> Result<Plugin, PluginError> {
        if path.is_empty() || path.len() > OMNI_MAX_PLUGIN_PATH {
            return Err(PluginError::BadPath);
        }
        let lib = Library::open(path)?;
        let verified = unsafe { verify(lib.symbol(b"omni_plugin_manifest\0"))? };
        Ok(Plugin {
            _lib: lib,
            name: verified.name,
            functions: verified.functions,
        })
    }

    /// The name the manifest declares, for diagnostics and tests.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Register every function into `engine`'s host-function table.
    ///
    /// On a failure partway through, everything this call already registered
    /// is rolled back, so a refused plugin leaves the engine exactly as it was.
    pub fn register(&self, engine: &mut Engine) -> Result<(), PluginError> {
        for (index, function) in self.functions.iter().enumerate() {
            let call = function.call;
            let outcome =
                engine.register_host_fn(&function.name, function.arity, move |engine, args| {
                    call_plugin(engine, call, args)
                });
            if let Err(error) = outcome {
                for earlier in &self.functions[..index] {
                    engine.unregister_host_fn(&earlier.name);
                }
                return Err(PluginError::RegisterFailed {
                    name: function.name.clone(),
                    error,
                });
            }
        }
        Ok(())
    }

    /// Drop every function this plugin registered. The library itself unloads
    /// when the owning [`Plugin`] drops.
    pub fn unregister(&self, engine: &mut Engine) {
        for function in &self.functions {
            engine.unregister_host_fn(&function.name);
        }
    }
}

/// Run one plugin function with the engine's argument contract, converting a
/// plugin panic into a host-call failure.
fn call_plugin(engine: &Engine, call: PluginCall, args: &[Value]) -> HostOutcome {
    if args.len() > crate::MAX_ARGS {
        // Core enforces arity at registration and at the call site; reaching
        // here means that invariant broke. Refusing beats writing past `buf`.
        return HostOutcome::Failure(b"more arguments than the ABI carries".to_vec());
    }
    let mut buf = [OmniValue::default(); crate::MAX_ARGS];
    for (slot, value) in buf.iter_mut().zip(args) {
        *slot = encode_argument(engine, *value);
    }
    let mut out = OmniValue::default();
    let argc = args.len() as u32;
    // Safety: `call` is the plugin's exported function, verified at load; the
    // owning `Plugin` keeps the library mapped for the engine's lifetime.
    // `buf` and `out` are exactly the shape the plugin ABI declares, and the
    // `C-unwind` ABI lets a panic unwind to the catch below instead of into
    // `omni_exec`'s foreign frames.
    let result = catch_unwind(AssertUnwindSafe(|| unsafe {
        call(ptr::null_mut(), buf.as_ptr(), argc, &mut out)
    }));
    match result {
        Ok(code) => crate::outcome(code, &out),
        Err(payload) => HostOutcome::Failure(panic_text(&*payload)),
    }
}

/// A plugin panic's message, best-effort: the payload is usually a `&str` or
/// `String` from the panic site, and anything else gets a fixed report.
fn panic_text(payload: &(dyn Any + Send)) -> Vec<u8> {
    if let Some(text) = payload.downcast_ref::<&str>() {
        return text.as_bytes().to_vec();
    }
    if let Some(text) = payload.downcast_ref::<String>() {
        return text.as_bytes().to_vec();
    }
    b"the plugin panicked".to_vec()
}

/// Read and check the manifest at `symbol`. The owning library stays mapped
/// for the whole call, so the verifier may read anything inside
/// [`OMNI_PLUGIN_VERIFY_WINDOW`] bytes on either side of the symbol — room
/// for the manifest, the table it points at, and every function name, which
/// is where a statically declared plugin keeps all of it. The linker is free
/// to place the table and the names *below* the manifest, so both directions
/// count.
///
/// # Safety
///
/// The caller guarantees the library exporting `symbol` is mapped, and
/// `symbol` is the address its loader returned.
unsafe fn verify(symbol: *mut c_void) -> Result<Verified, PluginError> {
    if symbol.is_null() {
        return Err(PluginError::MissingManifest);
    }
    // Safety: the caller's guarantee, widened only to the fixed window.
    unsafe {
        verify_manifest(Window {
            base: symbol.cast::<u8>(),
            len: OMNI_PLUGIN_VERIFY_WINDOW,
            floor: (symbol as usize).saturating_sub(OMNI_PLUGIN_VERIFY_WINDOW),
        })
    }
}

/// Verify a manifest from an owned byte slice — the door the test suite uses
/// to feed hostile manifests without building hostile libraries. `bytes` is
/// the whole readable window: the manifest, its function table, and its
/// function names must all live inside it, exactly as they must live inside
/// a real library's mapped image.
pub fn verify_bytes(bytes: &[u8]) -> Result<(), PluginError> {
    // Safety: the slice is readable for `bytes.len()`, and `verify_manifest`
    // bounds-checks every read against the window.
    unsafe {
        verify_manifest(Window {
            base: bytes.as_ptr(),
            len: bytes.len(),
            floor: bytes.as_ptr() as usize,
        })
    }
    .map(|_| ())
}

/// The readable region verification may touch. The manifest begins at `base`
/// and extends `len` bytes upward; `floor` is the lowest address any pointer
/// the manifest carries may point at. A real library's window reaches one
/// [`OMNI_PLUGIN_VERIFY_WINDOW`] below the symbol too, a byte slice's starts
/// exactly at its own first byte.
struct Window {
    base: *const u8,
    len: usize,
    floor: usize,
}

impl Window {
    /// Whether `[addr, addr + size)` lies inside the window.
    fn contains(&self, addr: *const u8, size: usize) -> bool {
        let start = addr as usize;
        start >= self.floor && start + size <= self.base as usize + self.len
    }

    /// How many bytes are readable from `addr` up to the window's top.
    fn above(&self, addr: *const u8) -> usize {
        self.base as usize + self.len - addr as usize
    }
}

/// Read and check the manifest at `window.base`. Every read is unaligned and
/// checked against the window first, every string is bounded and copied out —
/// so a hostile library can get itself refused, but cannot make this function
/// read past the window or keep a pointer alive past the load.
///
/// # Safety
///
/// Every byte of the window — `[floor, base + len)` — must be readable for
/// the duration of the call.
unsafe fn verify_manifest(window: Window) -> Result<Verified, PluginError> {
    let base = window.base;
    // Safety: the helpers below check their offsets against the window, which
    // is the caller's guarantee.
    let magic = unsafe { read_u32(&window, 0)? };
    if magic != OMNI_PLUGIN_MAGIC {
        return Err(PluginError::BadMagic(magic));
    }
    let size = unsafe { read_u32(&window, 4)? };
    if size as usize != size_of::<OmniPluginManifest>() {
        return Err(PluginError::BadSize(size));
    }
    let abi_version = unsafe { read_u32(&window, 8)? };
    if abi_version != OMNI_PLUGIN_ABI_VERSION {
        return Err(PluginError::BadAbiVersion(abi_version));
    }
    let omni_abi = unsafe { read_u32(&window, 12)? };
    if omni_abi != OMNI_ABI_VERSION {
        return Err(PluginError::AbiMismatch {
            plugin: omni_abi,
            engine: OMNI_ABI_VERSION,
        });
    }
    if window.len < 16 + OMNI_MAX_PLUGIN_NAME {
        return Err(PluginError::Truncated);
    }
    let mut name_bytes = [0u8; OMNI_MAX_PLUGIN_NAME];
    // Safety: the length check above keeps the copy inside the window.
    unsafe {
        ptr::copy_nonoverlapping(base.add(16), name_bytes.as_mut_ptr(), OMNI_MAX_PLUGIN_NAME);
    }
    let name_end = name_bytes
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(OMNI_MAX_PLUGIN_NAME);
    if name_end == 0 {
        return Err(PluginError::Unnamed);
    }
    let name = std::str::from_utf8(&name_bytes[..name_end]).map_err(|_| PluginError::BadName)?;
    let function_count = unsafe { read_u32(&window, 64)? };
    if function_count as usize > OMNI_MAX_PLUGIN_FUNCTIONS {
        return Err(PluginError::TooManyFunctions(function_count));
    }
    let functions = unsafe { read_word(&window, 72)? } as *const OmniPluginFn;
    if function_count > 0 && functions.is_null() {
        return Err(PluginError::BadFunctionTable);
    }

    let mut verified = Vec::with_capacity(function_count as usize);
    for index in 0..function_count as usize {
        // Safety: `functions.add` is arithmetic on the pointer the manifest
        // declared; the window check below bounds the read that follows it.
        let entry = unsafe { functions.add(index) }.cast::<u8>();
        if !window.contains(entry, size_of::<OmniPluginFn>()) {
            return Err(PluginError::Truncated);
        }
        // Safety: the window check above bounds the read.
        let entry = unsafe { ptr::read_unaligned(functions.add(index)) };
        let function_name = unsafe { read_function_name(&window, entry.name) }
            .ok_or(PluginError::BadFunctionName { index })?;
        let Some(call) = entry.call else {
            return Err(PluginError::NullFunction {
                index,
                name: function_name,
            });
        };
        if entry.arity > crate::MAX_ARGS as u32 {
            return Err(PluginError::ArityTooLarge {
                name: function_name,
                arity: entry.arity,
            });
        }
        if verified
            .iter()
            .any(|f: &PluginFunction| f.name == function_name)
        {
            return Err(PluginError::DuplicateName(function_name));
        }
        verified.push(PluginFunction {
            name: function_name,
            arity: entry.arity,
            call,
        });
    }
    Ok(Verified {
        name: name.to_string(),
        functions: verified,
    })
}

/// One unaligned `u32` at `base + offset`, refused when it would leave the
/// window.
///
/// # Safety
///
/// The window is readable.
unsafe fn read_u32(window: &Window, offset: usize) -> Result<u32, PluginError> {
    if window.len < offset + size_of::<u32>() {
        return Err(PluginError::Truncated);
    }
    // Safety: the check above keeps the read inside the window.
    Ok(unsafe { ptr::read_unaligned(window.base.add(offset).cast::<u32>()) })
}

/// One unaligned pointer-width word at `base + offset`, refused when it would
/// leave the window.
///
/// # Safety
///
/// The window is readable.
unsafe fn read_word(window: &Window, offset: usize) -> Result<usize, PluginError> {
    if window.len < offset + size_of::<usize>() {
        return Err(PluginError::Truncated);
    }
    // Safety: the check above keeps the read inside the window.
    Ok(unsafe { ptr::read_unaligned(window.base.add(offset).cast::<usize>()) })
}

/// Read a plugin function's NUL-terminated name into an owned `String`,
/// refusing anything that is missing, over-long, not UTF-8, or not a legal
/// identifier. `addr` must point inside the window; the scan reads at most
/// [`OMNI_MAX_PLUGIN_FN_NAME`] bytes and never past the window's top.
///
/// # Safety
///
/// The window is readable.
unsafe fn read_function_name(window: &Window, addr: *const c_char) -> Option<String> {
    let addr = addr.cast::<u8>();
    if !window.contains(addr, 1) {
        return None;
    }
    // Safety: `addr` is inside the window and the scan is clipped to it.
    let bytes = unsafe {
        std::slice::from_raw_parts(addr, OMNI_MAX_PLUGIN_FN_NAME.min(window.above(addr)))
    };
    let end = bytes.iter().position(|&byte| byte == 0)?;
    if end == 0 || end > crate::MAX_IDENTIFIER_LEN {
        return None;
    }
    std::str::from_utf8(&bytes[..end]).ok().map(str::to_owned)
}
