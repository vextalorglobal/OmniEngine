//! The `omni` command.
//!
//! Argument parsing is hand-rolled. Adding `clap` here would pull a dozen
//! crates into the workspace for three subcommands, against a drive with 7.5 GB
//! free and a core crate whose zero-dependency property is load-bearing.

use std::io::{Read, Write};
use std::process::ExitCode;

use omni_core::Engine;

const USAGE: &str = "\
omni — the OmniEngine command

USAGE:
    omni run <file.omni>     Run a program from a file
    omni eval <expression>   Evaluate one expression
    omni --version           Print the engine version
    omni --help              Show this message

EXIT CODES:
    0  success
    1  the program parsed but failed to run
    2  usage error, or the file could not be read
";

/// Exit code for an engine-level failure.
const EXIT_ENGINE: u8 = 1;
/// Exit code for a usage or I/O failure.
const EXIT_USAGE: u8 = 2;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match dispatch(&args) {
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
        Some("--version") => Ok(Some(format!(
            "omni {} (language v{})",
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
}
