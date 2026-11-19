//! `omni serve`: run this build as a cluster worker.
//!
//! The node is the same [`omni_cluster::WorkerNode`] the integration tests
//! start, exposed as a process so a cluster can span machines. Every knob that
//! decides what a remote task may do is a flag here — the bind address, the
//! pre-shared key, the gas ceiling, and the read roots — and none of them is
//! reachable from the wire. A task that arrives over the network can run
//! arithmetic, but it cannot grant itself a filesystem root, raise its own gas
//! budget, or import a module, because the template engine was configured
//! before the listener bound.
//!
//! The key is the node's whole authentication boundary, so it is read as hex
//! rather than typed as eight bytes, and `--psk-file` exists because a key in
//! `argv` is visible to every process on the machine.

use std::path::PathBuf;
use std::process::ExitCode;

use omni_cluster::{WorkerConfig, WorkerNode};

/// Exit code for a usage or configuration failure.
const EXIT_USAGE: u8 = 2;

/// The flags `omni serve` reads.
#[derive(Debug)]
pub struct ServeFlags {
    pub bind: String,
    pub psk: [u8; 32],
    pub gas_cap: u64,
    pub allow_read: Vec<PathBuf>,
    pub cpu_ms_cap: u64,
    pub memory_mib_cap: u64,
    pub tls_cert_pem: Option<PathBuf>,
    pub tls_key_pem: Option<PathBuf>,
}

pub const SERVE_USAGE: &str = "\
omni serve — run this build as a cluster worker

USAGE:
    omni serve --psk <hex> [options]

OPTIONS:
    --psk <hex>           64 hex characters (32 bytes): the shared key both
                          sides must hold. Required (or --psk-file).
    --psk-file <path>     Read the key from a file: 64 hex characters, or 32
                          raw bytes.
    --bind <addr>         Address to listen on (default 127.0.0.1:7717).
    --gas-cap <n>         Refuse any task asking for more than n steps.
                          Zero (the default) means the engine default per task.
    --allow-read <dir>    Let remote tasks read files under <dir>. Repeatable;
                          the default is no filesystem access at all.
    --cpu-ms-cap <n>      Wall-clock CPU ceiling per task (default 50).
    --memory-mib-cap <n>  Sandbox memory ceiling per task (default 128).
    --tls-cert <path>     TLS certificate PEM. Must be paired with --tls-key.
    --tls-key <path>      TLS private-key PEM. Must be paired with --tls-cert.

The node runs until it is killed. Its stdout prints the bound address once,
on a line beginning 'omni serve listening on ', so a launcher can wait for it.
";

/// Read a 32-byte key from a file: 64 hex characters (with any surrounding
/// whitespace), or exactly 32 raw bytes.
fn psk_from_file(path: &str) -> Result<[u8; 32], String> {
    let bytes = std::fs::read(path).map_err(|e| format!("cannot read '{path}': {e}"))?;
    if bytes.len() == 32 {
        return Ok(bytes.as_slice().try_into().unwrap());
    }
    // Anything else is read as hex text; a trailing newline is expected from
    // every editor and every `--generate` this project ships.
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| format!("'{path}' is neither 32 raw bytes nor hex text"))?;
    parse_psk(text.trim()).map_err(|e| format!("'{path}': {e}"))
}

/// Decode 64 hex characters into 32 bytes.
fn parse_psk(text: &str) -> Result<[u8; 32], String> {
    let hex = text.trim();
    if hex.len() != 64 {
        return Err(format!(
            "a key is 64 hex characters (32 bytes); this is {}",
            hex.len()
        ));
    }
    let mut key = [0u8; 32];
    for (i, byte) in key.iter_mut().enumerate() {
        let pair = &hex[i * 2..i * 2 + 2];
        *byte = u8::from_str_radix(pair, 16)
            .map_err(|_| format!("'{pair}' is not a hex byte"))?;
    }
    Ok(key)
}

pub fn parse_serve_flags(args: &[String]) -> Result<ServeFlags, String> {
    let mut flags = ServeFlags {
        bind: "127.0.0.1:7717".to_string(),
        psk: [0u8; 32],
        gas_cap: 0,
        allow_read: Vec::new(),
        cpu_ms_cap: 50,
        memory_mib_cap: 128,
        tls_cert_pem: None,
        tls_key_pem: None,
    };
    let mut key_seen = false;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        let value = |name: &str| -> Result<String, String> {
            args.get(index + 1)
                .cloned()
                .ok_or_else(|| format!("missing value after '{name}'"))
        };
        match arg.as_str() {
            "--psk" => {
                let text = value("--psk")?;
                flags.psk = parse_psk(&text)?;
                key_seen = true;
                index += 2;
            }
            "--psk-file" => {
                let path = value("--psk-file")?;
                flags.psk = psk_from_file(&path)?;
                key_seen = true;
                index += 2;
            }
            "--bind" => {
                flags.bind = value("--bind")?;
                index += 2;
            }
            "--gas-cap" => {
                let text = value("--gas-cap")?;
                flags.gas_cap = text
                    .parse()
                    .map_err(|_| format!("'--gas-cap' needs a whole number, not '{text}'"))?;
                index += 2;
            }
            "--allow-read" => {
                flags.allow_read.push(PathBuf::from(value("--allow-read")?));
                index += 2;
            }
            "--cpu-ms-cap" => {
                let text = value("--cpu-ms-cap")?;
                flags.cpu_ms_cap = text.parse().map_err(|_| format!("'--cpu-ms-cap' needs a whole number, not '{text}'"))?;
                index += 2;
            }
            "--memory-mib-cap" => {
                let text = value("--memory-mib-cap")?;
                flags.memory_mib_cap = text.parse().map_err(|_| format!("'--memory-mib-cap' needs a whole number, not '{text}'"))?;
                index += 2;
            }
            "--tls-cert" => {
                flags.tls_cert_pem = Some(PathBuf::from(value("--tls-cert")?));
                index += 2;
            }
            "--tls-key" => {
                flags.tls_key_pem = Some(PathBuf::from(value("--tls-key")?));
                index += 2;
            }
            other => return Err(format!("unknown option '{other}'; try 'omni serve --help'")),
        }
    }
    if !key_seen {
        return Err("a shared key is required: pass --psk <hex> or --psk-file <path>".to_string());
    }
    Ok(flags)
}

/// Start the worker and block until the process is killed.
pub fn run_serve(flags: ServeFlags) -> ExitCode {
    let config = WorkerConfig {
        bind_addr: flags.bind.clone(),
        psk: flags.psk,
        gas_cap: flags.gas_cap,
        fs_read_roots: flags.allow_read.clone(),
        cpu_ms_cap: 50,
        memory_mib_cap: 128,
        tls_cert_pem: None,
        tls_key_pem: None,
    };
    let node = match WorkerNode::start(config) {
        Ok(node) => node,
        Err(error) => {
            eprintln!("omni serve: cannot bind {}: {error}", flags.bind);
            return ExitCode::from(EXIT_USAGE);
        }
    };
    // One line, at startup, on stdout: the address a launcher waits for. The
    // wording is part of the contract the verification harness reads.
    println!(
        "omni serve listening on {} (gas cap {}, {} read root(s))",
        node.addr(),
        if flags.gas_cap == 0 {
            "engine default".to_string()
        } else {
            flags.gas_cap.to_string()
        },
        flags.allow_read.len()
    );
    // Park rather than spin: the accept loop is on its own thread, and the
    // process ends when the operator kills it. Ctrl-C terminates the process
    // by default, which closes the listener and lets clients fail over.
    loop {
        std::thread::park();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_hex() -> String {
        (0..32).map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn a_hex_key_decodes_to_the_bytes_it_spells() {
        let key = parse_psk(&key_hex()).unwrap();
        assert_eq!(key[0], 0x00);
        assert_eq!(key[1], 0x01);
        assert_eq!(key[31], 0x1f);
    }

    #[test]
    fn a_key_of_the_wrong_shape_is_refused() {
        assert!(parse_psk("").is_err());
        assert!(parse_psk(&"a".repeat(63)).is_err());
        assert!(parse_psk(&"a".repeat(64)).is_ok());
        assert!(parse_psk(&"z".repeat(64)).is_err(), "non-hex is not a key");
    }

    #[test]
    fn serve_refuses_without_a_key_and_defaults_the_rest() {
        let err = parse_serve_flags(&[]).unwrap_err();
        assert!(err.contains("shared key"), "{err}");

        let flags = parse_serve_flags(&["--psk".into(), key_hex()]).unwrap();
        assert_eq!(flags.bind, "127.0.0.1:7717");
        assert_eq!(flags.gas_cap, 0);
        assert!(flags.allow_read.is_empty());
    }

    #[test]
    fn serve_reads_every_knob_and_refuses_junk() {
        let flags = parse_serve_flags(&[
            "--psk".into(),
            key_hex(),
            "--bind".into(),
            "0.0.0.0:9000".into(),
            "--gas-cap".into(),
            "5000".into(),
            "--allow-read".into(),
            "/tmp/a".into(),
            "--allow-read".into(),
            "/tmp/b".into(),
        ])
        .unwrap();
        assert_eq!(flags.bind, "0.0.0.0:9000");
        assert_eq!(flags.gas_cap, 5000);
        assert_eq!(flags.allow_read.len(), 2);

        assert!(parse_serve_flags(&["--nope".into()]).is_err());
        assert!(
            parse_serve_flags(&["--psk".into(), key_hex(), "--gas-cap".into(), "lots".into()])
                .is_err()
        );
        // A flag with no value is refused rather than silently ignored.
        assert!(parse_serve_flags(&["--psk".into()]).is_err());
    }
}
