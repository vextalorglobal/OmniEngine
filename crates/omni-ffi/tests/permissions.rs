//! Contract tests for the ABI 5 permission exports, written as an external
//! consumer would: raw pointers, byte buffers, and no Rust-side conveniences.
//!
//! The three grants — `omni_engine_allow_fs_read`, `omni_engine_allow_fs_write`,
//! and `omni_engine_set_module_root` — are the sandbox's front door, so these
//! tests are the ones that stand behind "Python and JavaScript can configure a
//! sandbox safely". The rules under test: malformed arguments are refused with
//! `OMNI_ERR_ARG` and change nothing, a grant made from inside a callback is
//! refused with `OMNI_ERR_REENTRANT`, denials carry positioned error text, and
//! every engine keeps its own set of roots — including the forks behind
//! `omni_exec_parallel`, which inherit the parent's grants and nothing else.

use std::ffi::{c_char, c_void};
use std::path::PathBuf;

use omni::*;

fn engine() -> *mut OmniEngine {
    let handle = omni_engine_new();
    assert!(!handle.is_null(), "omni_engine_new returned null");
    handle
}

/// Evaluate and hand back the raw status plus value, mirroring a C caller.
fn call(engine: *mut OmniEngine, source: &str) -> (u32, OmniValue) {
    let mut out = OmniValue::default();
    let code = unsafe {
        omni_exec(
            engine,
            source.as_ptr().cast::<c_char>(),
            source.len(),
            &mut out,
        )
    };
    (code, out)
}

/// The most recent rendered engine error, as a host would show it.
fn last_error(engine: *mut OmniEngine) -> String {
    let mut buf = vec![0u8; 512];
    let n = unsafe { omni_last_error(engine, buf.as_mut_ptr().cast(), buf.len()) };
    buf.truncate(n);
    String::from_utf8(buf).unwrap()
}

/// Copy a string result out of engine-owned storage, as a C caller must.
fn copy_out(engine: *const OmniEngine, value: &OmniValue) -> Vec<u8> {
    assert_eq!(value.tag, OMNI_STRING, "not a string: {value:?}");
    let mut buf = vec![0u8; value.len as usize + 1];
    let n = unsafe { omni_str_copy(engine, value, buf.as_mut_ptr().cast(), buf.len()) };
    assert_eq!(n as u32, value.len, "copied {n} of {}", value.len);
    buf.truncate(n);
    buf
}

/// A unique scratch directory per test; removed at the end of each test.
fn temp_dir(tag: &str) -> PathBuf {
    let path =
        std::env::temp_dir().join(format!("omni-ffi-permissions-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn path_bytes(path: &str) -> (*const c_char, usize) {
    (path.as_ptr().cast::<c_char>(), path.len())
}

#[test]
fn every_malformed_grant_is_an_argument_error() {
    let engine = engine();
    unsafe {
        let (ok, ok_len) = path_bytes(".");
        let long = vec![b'a'; OMNI_MAX_PATH + 1];
        let long_path = (long.as_ptr().cast::<c_char>(), long.len());
        let non_utf8 = [0xFFu8, 0xFE];
        let non_utf8_path = (non_utf8.as_ptr().cast::<c_char>(), non_utf8.len());

        for (what, engine_ptr, path, len) in [
            ("null engine", std::ptr::null_mut(), ok, ok_len),
            ("null path", engine, std::ptr::null(), 1),
            ("empty path", engine, ok, 0),
            ("over-long path", engine, long_path.0, long_path.1),
            ("non-UTF-8 path", engine, non_utf8_path.0, non_utf8_path.1),
        ] {
            assert_eq!(
                omni_engine_allow_fs_read(engine_ptr, path, len),
                OMNI_ERR_ARG,
                "allow_fs_read ({what})"
            );
            assert_eq!(
                omni_engine_allow_fs_write(engine_ptr, path, len),
                OMNI_ERR_ARG,
                "allow_fs_write ({what})"
            );
            assert_eq!(
                omni_engine_set_module_root(engine_ptr, path, len),
                OMNI_ERR_ARG,
                "set_module_root ({what})"
            );
        }

        // A refused grant changes nothing, so the engine is still usable.
        let (code, value) = call(engine, "1 + 1");
        assert_eq!(code, OMNI_OK);
        assert_eq!(value.number(), Some(2.0));
        omni_engine_free(engine);
    }
}

#[test]
fn a_grant_needs_a_resolvable_directory() {
    let engine = engine();
    unsafe {
        let missing = format!("{}\\no-such-dir", temp_dir("missing").display());
        let (path, len) = path_bytes(&missing);
        assert_eq!(
            omni_engine_allow_fs_read(engine, path, len),
            OMNI_ERR_ARG,
            "a directory that does not exist cannot become a root"
        );
        assert_eq!(omni_engine_allow_fs_write(engine, path, len), OMNI_ERR_ARG);
        assert_eq!(omni_engine_set_module_root(engine, path, len), OMNI_ERR_ARG);

        // The failed grant left the sandbox untouched: reads stay denied.
        let (code, _) = call(engine, &format!("fs.read_text({missing:?})"));
        assert_eq!(code, OMNI_ERR_RUNTIME);
        assert!(
            last_error(engine).ends_with(
                "fs.read_text is disabled: this engine has no filesystem read permission"
            ),
            "got: {}",
            last_error(engine)
        );
        omni_engine_free(engine);
    }
}

#[test]
fn read_permission_fails_closed_then_a_grant_opens_the_root() {
    let engine = engine();
    let root_a = temp_dir("read-a");
    let root_b = temp_dir("read-b");
    let file_a = root_a.join("a.txt").display().to_string();
    let file_b = root_b.join("b.txt").display().to_string();
    std::fs::write(root_a.join("a.txt"), "from a").unwrap();
    std::fs::write(root_b.join("b.txt"), "from b").unwrap();

    unsafe {
        // No roots at all: the fail-closed default, named for the caller.
        let (code, _) = call(engine, &format!("fs.read_text({file_a:?})"));
        assert_eq!(code, OMNI_ERR_RUNTIME);
        assert!(
            last_error(engine).ends_with(
                "fs.read_text is disabled: this engine has no filesystem read permission"
            ),
            "got: {}",
            last_error(engine)
        );

        // One root opens exactly that root.
        let root_a_str = root_a.display().to_string();
        let (path, len) = path_bytes(&root_a_str);
        assert_eq!(omni_engine_allow_fs_read(engine, path, len), OMNI_OK);
        let (code, value) = call(engine, &format!("fs.read_text({file_a:?})"));
        assert_eq!(code, OMNI_OK);
        assert_eq!(copy_out(engine, &value), b"from a");

        // A sibling root stays closed, and the refusal names the request path.
        let (code, _) = call(engine, &format!("fs.read_text({file_b:?})"));
        assert_eq!(code, OMNI_ERR_RUNTIME);
        assert!(
            last_error(engine).ends_with("is outside the allowed read roots"),
            "got: {}",
            last_error(engine)
        );

        // Granting the second root is what opens it — nothing else.
        let root_b_str = root_b.display().to_string();
        let (path, len) = path_bytes(&root_b_str);
        assert_eq!(omni_engine_allow_fs_read(engine, path, len), OMNI_OK);
        let (code, value) = call(engine, &format!("fs.read_text({file_b:?})"));
        assert_eq!(code, OMNI_OK);
        assert_eq!(copy_out(engine, &value), b"from b");
        omni_engine_free(engine);
    }
    std::fs::remove_dir_all(&root_a).unwrap();
    std::fs::remove_dir_all(&root_b).unwrap();
}

#[test]
fn write_permission_fails_closed_then_a_grant_opens_the_root() {
    let engine = engine();
    let root_a = temp_dir("write-a");
    let root_b = temp_dir("write-b");
    let file_a = root_a.join("a.txt").display().to_string();
    let file_b = root_b.join("b.txt").display().to_string();

    unsafe {
        let (code, _) = call(engine, &format!("fs.write_text({file_a:?}, \"x\")"));
        assert_eq!(code, OMNI_ERR_RUNTIME);
        assert!(
            last_error(engine).ends_with(
                "fs.write_text is disabled: this engine has no filesystem write permission"
            ),
            "got: {}",
            last_error(engine)
        );

        let root_a_str = root_a.display().to_string();
        let (path, len) = path_bytes(&root_a_str);
        assert_eq!(omni_engine_allow_fs_write(engine, path, len), OMNI_OK);
        let (code, _) = call(engine, &format!("fs.write_text({file_a:?}, \"written\")"));
        assert_eq!(code, OMNI_OK);
        assert_eq!(
            std::fs::read_to_string(root_a.join("a.txt")).unwrap(),
            "written",
            "the granted write reached the disk"
        );

        let (code, _) = call(engine, &format!("fs.write_text({file_b:?}, \"x\")"));
        assert_eq!(code, OMNI_ERR_RUNTIME);
        assert!(
            last_error(engine).ends_with("is outside the allowed write roots"),
            "got: {}",
            last_error(engine)
        );
        assert!(
            !root_b.join("b.txt").exists(),
            "a denied write touched the disk"
        );

        let root_b_str = root_b.display().to_string();
        let (path, len) = path_bytes(&root_b_str);
        assert_eq!(omni_engine_allow_fs_write(engine, path, len), OMNI_OK);
        let (code, _) = call(engine, &format!("fs.write_text({file_b:?}, \"owned\")"));
        assert_eq!(code, OMNI_OK);
        assert_eq!(
            std::fs::read_to_string(root_b.join("b.txt")).unwrap(),
            "owned"
        );
        omni_engine_free(engine);
    }
    std::fs::remove_dir_all(&root_a).unwrap();
    std::fs::remove_dir_all(&root_b).unwrap();
}

#[test]
fn the_module_root_gates_imports_and_a_grant_opens_them() {
    let engine = engine();
    let root = temp_dir("module");
    std::fs::write(root.join("helper.omni"), "let answer = 40 + 2").unwrap();

    unsafe {
        let (code, _) = call(engine, "import \"helper\"\nhelper.answer");
        assert_eq!(code, OMNI_ERR_RUNTIME);
        assert!(
            last_error(engine).ends_with("import is disabled: this engine has no module root"),
            "got: {}",
            last_error(engine)
        );

        let root_str = root.display().to_string();
        let (path, len) = path_bytes(&root_str);
        assert_eq!(omni_engine_set_module_root(engine, path, len), OMNI_OK);
        let (code, value) = call(engine, "import \"helper\"\nhelper.answer");
        assert_eq!(code, OMNI_OK);
        assert_eq!(value.number(), Some(42.0));

        // The root gates the module namespace, not the disk: a missing module
        // is named for the requester.
        let (code, _) = call(engine, "import \"other\"");
        assert_eq!(code, OMNI_ERR_RUNTIME);
        let message = last_error(engine);
        assert!(message.contains("cannot import 'other'"), "got: {message}");
        assert!(
            message.ends_with("no such module under the module root"),
            "got: {message}"
        );
        omni_engine_free(engine);
    }
    std::fs::remove_dir_all(&root).unwrap();
}

/// A registered callback that tries to grant itself a read root. The engine it
/// aims at arrives through `user_data`, exactly as a C host would pass it.
unsafe extern "C" fn grant_from_callback(
    user_data: *mut c_void,
    _args: *const OmniValue,
    _argc: u32,
    out: *mut OmniValue,
) -> i32 {
    let engine = user_data.cast::<OmniEngine>();
    let path = b".";
    // The re-entrancy guard fires before the path is even canonicalized, so
    // any readable path proves the rule.
    let code =
        unsafe { omni_engine_allow_fs_read(engine, path.as_ptr().cast::<c_char>(), path.len()) };
    unsafe {
        *out = OmniValue {
            tag: OMNI_NUMBER,
            len: 0,
            payload: OmniPayload {
                number: code as f64,
            },
        };
    }
    OMNI_HOST_OK
}

#[test]
fn a_grant_from_inside_a_callback_is_reentrancy_blocked() {
    let engine = engine();
    unsafe {
        let name = b"rebel";
        assert_eq!(
            omni_register_host_fn(
                engine,
                name.as_ptr().cast::<c_char>(),
                name.len(),
                0,
                Some(grant_from_callback),
                engine.cast::<c_void>(),
            ),
            OMNI_OK
        );
        let (code, value) = call(engine, "rebel()");
        assert_eq!(code, OMNI_OK);
        assert_eq!(
            value.number(),
            Some(OMNI_ERR_REENTRANT as f64),
            "a callback may not reconfigure the engine it is running inside"
        );
        omni_engine_free(engine);
    }
}

#[test]
fn engines_never_share_grants() {
    let root = temp_dir("isolation");
    std::fs::write(root.join("n.txt"), "7").unwrap();
    let file = root.join("n.txt").display().to_string();

    let granted = engine();
    let stranger = engine();
    unsafe {
        let root_str = root.display().to_string();
        let (path, len) = path_bytes(&root_str);
        assert_eq!(omni_engine_allow_fs_read(granted, path, len), OMNI_OK);

        let (code, value) = call(granted, &format!("fs.read_text({file:?})"));
        assert_eq!(code, OMNI_OK);
        assert_eq!(copy_out(granted, &value), b"7");

        let (code, _) = call(stranger, &format!("fs.read_text({file:?})"));
        assert_eq!(code, OMNI_ERR_RUNTIME);
        assert!(
            last_error(stranger).ends_with(
                "fs.read_text is disabled: this engine has no filesystem read permission"
            ),
            "a grant on one engine must never leak into another; got: {}",
            last_error(stranger)
        );
        omni_engine_free(granted);
        omni_engine_free(stranger);
    }
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn parallel_workers_inherit_the_parents_grants_and_a_fresh_sibling_stays_denied() {
    let root = temp_dir("parallel");
    std::fs::write(root.join("n.txt"), "7").unwrap();
    let file = root.join("n.txt").display().to_string();
    let program = format!("fs.read_text({file:?})");

    let granted = engine();
    unsafe {
        let root_str = root.display().to_string();
        let (path, len) = path_bytes(&root_str);
        assert_eq!(omni_engine_allow_fs_read(granted, path, len), OMNI_OK);

        // Each worker is a fork of the granting engine, so the grant rides along.
        let sources = [program.as_str(), program.as_str()];
        let ptrs: Vec<*const c_char> = sources.iter().map(|s| s.as_ptr().cast()).collect();
        let lens: Vec<usize> = sources.iter().map(|s| s.len()).collect();
        let mut out = vec![OmniValue::default(); 2];
        let mut statuses = vec![u32::MAX; 2];
        let code = omni_exec_parallel(
            granted,
            ptrs.as_ptr(),
            lens.as_ptr(),
            2,
            2,
            out.as_mut_ptr(),
            statuses.as_mut_ptr(),
        );
        assert_eq!(code, OMNI_OK);
        for (value, status) in out.iter().zip(&statuses) {
            assert_eq!(*status, OMNI_OK);
            assert_eq!(copy_out(granted, value), b"7");
        }

        // A sibling engine with no grants answers the same program with the
        // fail-closed default: no roots, no reads.
        let stranger = engine();
        let (code, _) = call(stranger, &program);
        assert_eq!(code, OMNI_ERR_RUNTIME);
        assert!(
            last_error(stranger).ends_with(
                "fs.read_text is disabled: this engine has no filesystem read permission"
            ),
            "got: {}",
            last_error(stranger)
        );
        omni_engine_free(stranger);
        omni_engine_free(granted);
    }
    std::fs::remove_dir_all(&root).unwrap();
}
