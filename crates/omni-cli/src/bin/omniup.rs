//! `omniup` — the safe updater for the `omni` command.
//!
//! It is a separate binary on purpose. The thing being replaced is
//! `omni` itself; a second executable that only knows how to check, download
//! and swap files can do that to the command it ships next to without the
//! command having to trust a flag in order to survive being rewritten.
//! `omni --update` exists too, and delegates to the same code in
//! [`omni_cli::updater`].
//!
//! ```text
//! omniup check
//! omniup update --yes
//! omniup update --dry-run --manifest file:///srv/omni-update.json
//! ```

use std::path::PathBuf;
use std::process::ExitCode;

use omni_cli::updater::{
    self, ABI_VERSION, CURRENT_VERSION, EXIT_ABI_MISMATCH, UpdateError, UpdateOptions,
};

const USAGE: &str = "\
omniup — update the OmniEngine command

USAGE:
    omniup check [--manifest <url>]   Report whether a newer omni exists
    omniup update [options]           Download, verify, and install a newer omni
    omniup --version                  Print the updater version
    omniup --help                     Show this message

UPDATE OPTIONS:
    --manifest <url>   Manifest to read (default: the latest GitHub release,
                       or $OMNI_UPDATE_URL)
    --target <path>    Binary to replace (default: omni next to omniup)
    --dry-run          Stop after checking; exit 10 if an update would apply
    -y, --yes          Do not ask for confirmation

EXIT CODES:
     0  up to date, or the update was installed
     2  usage, fetch, or manifest failure
     3  the download failed verification or could not be installed
    10  a newer release exists (check, or update --dry-run)
    11  the release speaks a different ABI
";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    Check,
    Update,
    Version,
    Help,
}

#[derive(Debug)]
struct Args {
    source: String,
    target: Option<PathBuf>,
    yes: bool,
    dry_run: bool,
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match parse(&argv) {
        Ok((Command::Help, _)) => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        Ok((Command::Version, _)) => {
            println!("omniup {CURRENT_VERSION} (ABI {ABI_VERSION})");
            ExitCode::SUCCESS
        }
        Ok((Command::Check, args)) => run_check(&args),
        Ok((Command::Update, args)) => run_update(args),
        Err(error) => fail(&error),
    }
}

fn parse(argv: &[String]) -> Result<(Command, Args), UpdateError> {
    let (command, rest) = match argv.first().map(String::as_str) {
        Some("check") => (Command::Check, &argv[1..]),
        Some("update") => (Command::Update, &argv[1..]),
        Some("--version" | "-V") => return Ok((Command::Version, default_args())),
        Some("--help" | "-h") | None => return Ok((Command::Help, default_args())),
        Some(other) => {
            return Err(UpdateError::Usage(format!(
                "unknown command '{other}'; try 'omniup --help'"
            )));
        }
    };

    let mut args = default_args();
    let mut index = 0;
    while index < rest.len() {
        let arg = &rest[index];
        match arg.as_str() {
            "-y" | "--yes" => args.yes = true,
            "--dry-run" => args.dry_run = true,
            "--help" | "-h" => return Ok((Command::Help, args)),
            "--manifest" | "--target" => {
                let value = rest
                    .get(index + 1)
                    .ok_or_else(|| UpdateError::Usage(format!("missing value after '{arg}'")))?;
                set_value(&mut args, arg, value)?;
                index += 1;
            }
            other if other.starts_with("--manifest=") => {
                set_value(&mut args, "--manifest", &other["--manifest=".len()..])?;
            }
            other if other.starts_with("--target=") => {
                set_value(&mut args, "--target", &other["--target=".len()..])?;
            }
            other => {
                return Err(UpdateError::Usage(format!(
                    "unknown option '{other}'; try 'omniup --help'"
                )));
            }
        }
        index += 1;
    }
    Ok((command, args))
}

fn default_args() -> Args {
    Args {
        source: updater::default_manifest_source(),
        target: None,
        yes: false,
        dry_run: false,
    }
}

fn set_value(args: &mut Args, flag: &str, value: &str) -> Result<(), UpdateError> {
    if value.is_empty() {
        return Err(UpdateError::Usage(format!(
            "'{flag}' needs a non-empty value"
        )));
    }
    match flag {
        "--manifest" => args.source = value.to_string(),
        "--target" => args.target = Some(PathBuf::from(value)),
        _ => unreachable!("set_value is only called for parsed flags"),
    }
    Ok(())
}

fn run_check(args: &Args) -> ExitCode {
    match updater::check(&args.source) {
        Ok(outcome) => {
            println!("{}", updater::check_summary(&outcome));
            ExitCode::from(updater::status_exit_code(outcome.status))
        }
        Err(error) => fail(&error),
    }
}

fn run_update(args: Args) -> ExitCode {
    let target = match args.target {
        Some(target) => target,
        None => match default_target() {
            Ok(target) => target,
            Err(error) => return fail(&error),
        },
    };
    let options = UpdateOptions {
        source: args.source,
        target,
        assume_yes: args.yes,
        dry_run: args.dry_run,
    };

    match updater::run_update(&options) {
        Ok(report) => {
            let (code, message) = updater::update_summary(&report);
            if code == EXIT_ABI_MISMATCH {
                eprintln!("omniup: {message}");
            } else {
                println!("{message}");
            }
            ExitCode::from(code)
        }
        Err(error) => fail(&error),
    }
}

/// The `omni` that was installed next to this updater.
fn default_target() -> Result<PathBuf, UpdateError> {
    let exe = std::env::current_exe()
        .map_err(|e| UpdateError::Usage(format!("cannot locate this executable: {e}")))?;
    let dir = exe.parent().ok_or_else(|| {
        UpdateError::Usage(format!("'{}' has no parent directory", exe.display()))
    })?;
    Ok(dir.join(if cfg!(windows) { "omni.exe" } else { "omni" }))
}

fn fail(error: &UpdateError) -> ExitCode {
    eprintln!("omniup: {error}");
    ExitCode::from(error.exit_code())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn check_accepts_a_manifest_as_a_separate_word_or_one() {
        let (command, parsed) =
            parse(&args(&["check", "--manifest", "file:///tmp/m.json"])).unwrap();
        assert_eq!(command, Command::Check);
        assert_eq!(parsed.source, "file:///tmp/m.json");

        let (_, parsed) = parse(&args(&["update", "--manifest=file:///tmp/m.json"])).unwrap();
        assert_eq!(parsed.source, "file:///tmp/m.json");
    }

    #[test]
    fn update_flags_are_collected() {
        let (command, parsed) =
            parse(&args(&["update", "--yes", "--dry-run", "--target", "omni"])).unwrap();
        assert_eq!(command, Command::Update);
        assert!(parsed.yes);
        assert!(parsed.dry_run);
        assert_eq!(parsed.target.as_deref(), Some(std::path::Path::new("omni")));
        assert!(!parsed.source.is_empty());
    }

    #[test]
    fn the_default_source_follows_the_environment() {
        // The default is whatever the environment says; this only asserts it is
        // never empty, because an empty URL is never a useful default.
        assert!(!default_args().source.is_empty());
    }

    #[test]
    fn help_version_and_nonsense() {
        assert_eq!(parse(&[]).unwrap().0, Command::Help);
        assert_eq!(parse(&args(&["--help"])).unwrap().0, Command::Help);
        assert_eq!(parse(&args(&["-V"])).unwrap().0, Command::Version);
        assert!(matches!(
            parse(&args(&["upgrade"])),
            Err(UpdateError::Usage(_))
        ));
        assert!(matches!(
            parse(&args(&["update", "--nope"])),
            Err(UpdateError::Usage(_))
        ));
        assert!(matches!(
            parse(&args(&["update", "--manifest"])),
            Err(UpdateError::Usage(_))
        ));
    }

    #[test]
    fn the_default_target_sits_next_to_the_updater() {
        let target = default_target().unwrap();
        let expected = if cfg!(windows) { "omni.exe" } else { "omni" };
        assert_eq!(target.file_name().unwrap(), expected);
        assert_eq!(target.parent(), std::env::current_exe().unwrap().parent());
    }
}
