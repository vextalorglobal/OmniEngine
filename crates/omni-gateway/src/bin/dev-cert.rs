use std::fs;
use std::path::PathBuf;

fn main() {
    if let Err(error) = run() {
        eprintln!("omni-gateway-dev-cert: {error}");
        std::process::exit(2);
    }
}

fn run() -> Result<(), String> {
    let mut args = std::env::args_os().skip(1);
    let cert_path = args
        .next()
        .map(PathBuf::from)
        .ok_or_else(|| "usage: omni-gateway-dev-cert <cert.pem> <key.pem>".to_string())?;
    let key_path = args
        .next()
        .map(PathBuf::from)
        .ok_or_else(|| "usage: omni-gateway-dev-cert <cert.pem> <key.pem>".to_string())?;
    if args.next().is_some() {
        return Err("usage: omni-gateway-dev-cert <cert.pem> <key.pem>".to_string());
    }

    let generated =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string(), "127.0.0.1".to_string()])
            .map_err(|error| format!("could not generate local TLS certificate: {error}"))?;

    for path in [&cert_path, &key_path] {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
        }
    }

    fs::write(&cert_path, generated.cert.pem())
        .map_err(|error| format!("cannot write {}: {error}", cert_path.display()))?;
    fs::write(&key_path, generated.key_pair.serialize_pem())
        .map_err(|error| format!("cannot write {}: {error}", key_path.display()))?;

    println!("Generated local development TLS certificate for localhost and 127.0.0.1.");
    Ok(())
}
