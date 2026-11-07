//! The `omni` command.
//!
//! Argument parsing is hand-rolled. Adding `clap` here would pull a dozen
//! crates into the workspace for a handful of subcommands, against a drive with
//! 7.5 GB free and a core crate whose zero-dependency property is load-bearing.

use std::io::{Read, Write};
use std::process::ExitCode;

use omni_cli::updater::{self, ABI_VERSION, EXIT_ABI_MISMATCH, UpdateError, UpdateOptions};
use omni_core::Engine;

const USAGE: &str = "\
omni — the OmniEngine command

USAGE:
    omni run <file.omni>     Run a program from a file
    omni eval <expression>   Evaluate one expression
    omni check-update        Report whether a newer omni exists
    omni update              Download, verify, and install a newer omni
    omni --version           Print the engine version
    omni --help              Show this message

UPDATE OPTIONS:
    --manifest <url>   Manifest to read (default: the latest GitHub release,
                       or $OMNI_UPDATE_URL)
    --dry-run          Stop after checking; exit 10 if an update would apply
    -y, --yes          Do not ask for confirmation

EXIT CODES:
     0  success, or already up to date
     1  the program parsed but failed to run
     2  usage error, the file could not be read, or the check failed
     3  the update failed verification or could not be installed
    10  a newer release exists (check-update, or update --dry-run)
    11  the release speaks a different ABI
";

/// Exit code for an engine-level failure.
const EXIT_ENGINE: u8 = 1;
/// Exit code for a usage or I/O failure.
const EXIT_USAGE: u8 = 2;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("check-update" | "--check-update") => return check_update_command(&args[1..]),
        Some("update" | "--update") => return update_command(&args[1..]),
        _ => {}
    }
    dispatch_exit(&args)
}

fn dispatch_exit(args: &[String]) -> ExitCode {
    match dispatch(args) {
        Ok(Some(out)) => {
            let mut stdout = std::io::stdout();
            let _ = writeln!(stdout, "{out}");
            ExitCode::SUCCESS
        }
        Ok(None) => ExitCode::SUCCESS,
        Err(message) => {
            let mut stderr = std::io::stderr();
            let _ = writeln!(stderr, "omni: {message}");
            message_exit_code(&message)
        }
    }
}

/// A usage/IO complaint is distinguishable from an engine one by its prefix, so
/// the caller can pick an exit code without threading an enum through `dispatch`.
fn message_exit_code(message: &str) -> ExitCode {
    if message.starts_with("[Line ") || message.contains(": [Line ") {
        ExitCode::from(EXIT_ENGINE)
    } else {
        ExitCode::from(EXIT_USAGE)
    }
}

fn dispatch(args: &[String]) -> Result<Option<String>, String> {
    match args.first().map(String::as_str) {
        None | Some("--help") | Some("-h") => {
            // Help goes to stdout through the normal result path.
            print!("{USAGE}");
            Ok(None)
        }
        Some("--version" | "-V") => Ok(Some(format!(
            "omni {} (language v{}, ABI {ABI_VERSION})",
            env!("CARGO_PKG_VERSION"),
            omni_core::LANGUAGE_VERSION
        ))),
        Some("run") => {
            let path = args.get(1).ok_or("missing file path after 'run'")?;
            let source = read_file(path)?;
            evaluate(&source, Some(path))
        }
        Some("eval") => {
            let source = args
                .get(1)
                .filter(|s| !s.is_empty())
                .ok_or("missing expression after 'eval'")?;
            evaluate(source, None)
        }
        Some(other) => Err(format!("unknown command '{other}'; try 'omni --help'")),
    }
}

/// The flags `check-update` and `update` share.
#[derive(Debug)]
struct UpdateFlags {
    source: String,
    yes: bool,
    dry_run: bool,
}

fn parse_update_flags(args: &[String]) -> Result<UpdateFlags, UpdateError> {
    let mut flags = UpdateFlags {
        source: updater::default_manifest_source(),
        yes: false,
        dry_run: false,
    };
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        match arg.as_str() {
            "-y" | "--yes" => flags.yes = true,
            "--dry-run" => flags.dry_run = true,
            "--manifest" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| UpdateError::Usage("missing value after '--manifest'".into()))?;
                set_manifest(&mut flags, value)?;
                index += 1;
            }
            other if other.starts_with("--manifest=") => {
                set_manifest(&mut flags, &other["--manifest=".len()..])?;
            }
            other => {
                return Err(UpdateError::Usage(format!(
                    "unknown option '{other}'; try 'omni --help'"
                )));
            }
        }
        index += 1;
    }
    Ok(flags)
}

fn set_manifest(flags: &mut UpdateFlags, value: &str) -> Result<(), UpdateError> {
    if value.is_empty() {
        return Err(UpdateError::Usage(
            "'--manifest' needs a non-empty value".into(),
        ));
    }
    flags.source = value.to_string();
    Ok(())
}

fn check_update_command(args: &[String]) -> ExitCode {
    let result = parse_update_flags(args).and_then(|flags| updater::check(&flags.source));
    match result {
        Ok(outcome) => {
            println!("{}", updater::check_summary(&outcome));
            ExitCode::from(updater::status_exit_code(outcome.status))
        }
        Err(error) => update_failure(&error),
    }
}

fn update_command(args: &[String]) -> ExitCode {
    let flags = match parse_update_flags(args) {
        Ok(flags) => flags,
        Err(error) => return update_failure(&error),
    };
    // `omni update` replaces this very binary; `omniup --target` exists for
    // updating a copy.
    let target = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("omni: cannot locate this executable: {error}");
            return ExitCode::from(EXIT_USAGE);
        }
    };
    let options = UpdateOptions {
        source: flags.source,
        target,
        assume_yes: flags.yes,
        dry_run: flags.dry_run,
    };
    match updater::run_update(&options) {
        Ok(report) => {
            let (code, message) = updater::update_summary(&report);
            if code == EXIT_ABI_MISMATCH {
                eprintln!("omni: {message}");
            } else {
                println!("{message}");
            }
            ExitCode::from(code)
        }
        Err(error) => update_failure(&error),
    }
}

fn update_failure(error: &UpdateError) -> ExitCode {
    eprintln!("omni: {error}");
    ExitCode::from(error.exit_code())
}

fn read_file(path: &str) -> Result<String, String> {
    let mut buf = Vec::new();
    let mut file = std::fs::File::open(path).map_err(|e| format!("cannot open '{path}': {e}"))?;
    file.read_to_end(&mut buf)
        .map_err(|e| format!("cannot read '{path}': {e}"))?;
    String::from_utf8(buf).map_err(|_| format!("'{path}' is not valid UTF-8"))
}

/// Run `source`, mapping an engine error onto the `[Line N, Col M]: …` prefix
/// that [`message_exit_code`] looks for. `path` only appears in diagnostics.
fn evaluate(source: &str, path: Option<&str>) -> Result<Option<String>, String> {
    let mut engine = Engine::new();
    match engine.exec(source) {
        // Rendered through the engine, not `Value`'s own `Display`: a string box
        // prints as `<string>` on purpose, because following its address is
        // something only the engine that owns the bytes may do. A CLI is exactly
        // where a program's string result should be readable, so it asks the
        // owner.
        Ok(value) => Ok(Some(engine.render(value))),
        Err(error) => Err(match path {
            Some(p) => format!("{p}: {error}"),
            None => error.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eval_produces_the_slice_answer() {
        assert_eq!(
            dispatch(&["eval".into(), "40 + 2".into()])
                .unwrap()
                .as_deref(),
            Some("42")
        );
    }

    #[test]
    fn version_and_help_are_not_errors() {
        assert!(dispatch(&["--version".into()]).unwrap().is_some());
        assert!(dispatch(&[]).is_ok());
    }

    #[test]
    fn version_names_the_abi_it_speaks() {
        let version = dispatch(&["--version".into()]).unwrap().unwrap();
        assert!(version.contains(&format!("ABI {ABI_VERSION}")), "{version}");
        assert!(version.contains(env!("CARGO_PKG_VERSION")), "{version}");
    }

    #[test]
    fn engine_failures_carry_the_positioned_prefix() {
        let e = dispatch(&["eval".into(), "1 / 0".into()]).unwrap_err();
        assert!(e.starts_with("[Line 1, Col 3]: "), "got {e}");
    }

    #[test]
    fn usage_failures_are_distinguishable() {
        assert_eq!(
            message_exit_code("[Line 1, Col 3]: division by zero"),
            ExitCode::from(EXIT_ENGINE)
        );
        assert_eq!(
            message_exit_code("missing file path"),
            ExitCode::from(EXIT_USAGE)
        );
        // `omni run` reports the path before the engine's position prefix.
        assert_eq!(
            message_exit_code("x.omni: [Line 3, Col 1]: division by zero"),
            ExitCode::from(EXIT_ENGINE)
        );
    }

    #[test]
    fn unknown_command_and_missing_argument_are_usage_errors() {
        assert!(
            dispatch(&["nope".into()])
                .unwrap_err()
                .contains("unknown command")
        );
        assert!(
            dispatch(&["run".into()])
                .unwrap_err()
                .contains("missing file")
        );
        assert!(
            dispatch(&["eval".into(), "".into()])
                .unwrap_err()
                .contains("missing expression")
        );
    }

    #[test]
    fn integral_values_render_without_a_trailing_zero() {
        assert_eq!(evaluate("40 + 2", None).unwrap().as_deref(), Some("42"));
        assert_eq!(evaluate("2.5", None).unwrap().as_deref(), Some("2.5"));
        assert_eq!(evaluate("-0.0", None).unwrap().as_deref(), Some("0"));
    }

    /// The point of routing through `Engine::render`: a string result is the
    /// text, not the placeholder `Value`'s own `Display` prints for a box it
    /// cannot prove is live.
    #[test]
    fn a_string_result_prints_its_content() {
        assert_eq!(
            evaluate(r#"let who = "world"; "hello, " + who"#, None)
                .unwrap()
                .as_deref(),
            Some("hello, world")
        );
        assert_eq!(evaluate(r#""""#, None).unwrap().as_deref(), Some(""));
        assert_eq!(
            evaluate(r#""tab\there""#, None).unwrap().as_deref(),
            Some("tab\there")
        );
    }

    #[test]
    fn update_flags_accept_both_spellings_of_a_manifest() {
        let parsed = parse_update_flags(&["--manifest".into(), "file:///m.json".into()]).unwrap();
        assert_eq!(parsed.source, "file:///m.json");
        assert!(!parsed.yes && !parsed.dry_run);

        let parsed =
            parse_update_flags(&["--manifest=file:///m.json".into(), "--yes".into()]).unwrap();
        assert_eq!(parsed.source, "file:///m.json");
        assert!(parsed.yes);
    }

    #[test]
    fn update_flags_refuse_nonsense() {
        assert!(matches!(
            parse_update_flags(&["--frobnicate".into()]),
            Err(UpdateError::Usage(_))
        ));
        assert!(matches!(
            parse_update_flags(&["--manifest".into()]),
            Err(UpdateError::Usage(_))
        ));
        assert!(matches!(
            parse_update_flags(&["--manifest=".into()]),
            Err(UpdateError::Usage(_))
        ));
    }
}
