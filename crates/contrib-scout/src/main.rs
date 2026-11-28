mod args;
mod commands;
mod render;

use clap::error::ErrorKind;
use clap::Parser;
use contrib_scout_core::ScoutError;
use std::{io, process::ExitCode};

fn main() -> ExitCode {
    // Argument errors exit 2 (with the usage envelope under --json / --ndjson);
    // --help and --version exit 0.
    let cli = match args::Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => return parse_failure(error),
    };
    match commands::run(&cli) {
        Ok(report) => match render::write(&report, &cli) {
            Ok(()) => ExitCode::from(report.exit.code()),
            Err(error) => fail(&error, &cli),
        },
        Err(error) => fail(&error, &cli),
    }
}

/// An argument error from clap. In JSON modes the usage error envelope goes
/// to stdout as for any other usage error; clap's message goes to stderr.
fn parse_failure(error: clap::Error) -> ExitCode {
    let informational = matches!(
        error.kind(),
        ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
    );
    let json = std::env::args_os()
        .skip(1)
        .take_while(|arg| arg != "--")
        .any(|arg| arg == "--json" || arg == "--ndjson");
    if informational || !json {
        error.exit();
    }
    let message = if error.kind() == ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand {
        // clap renders the whole help page for this kind.
        "a subcommand or argument is missing; see --help".to_owned()
    } else {
        let rendered = error.render().to_string();
        let message = rendered.trim();
        message
            .strip_prefix("error: ")
            .unwrap_or(message)
            .to_owned()
    };
    render::write_error_envelope(&ScoutError::Usage(message).into());
    let _ = error.print();
    ExitCode::from(2)
}

fn fail(error: &anyhow::Error, cli: &args::Cli) -> ExitCode {
    if is_broken_pipe(error) {
        return ExitCode::SUCCESS;
    }
    ExitCode::from(render::write_error(error, cli.json || cli.ndjson))
}

/// The reader of stdout went away. JSON output is written through
/// serde_json, which wraps the I/O error in its own error type.
fn is_broken_pipe(error: &anyhow::Error) -> bool {
    let kind = error
        .downcast_ref::<io::Error>()
        .map(io::Error::kind)
        .or_else(|| {
            error
                .downcast_ref::<serde_json::Error>()
                .and_then(serde_json::Error::io_error_kind)
        });
    kind == Some(io::ErrorKind::BrokenPipe)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ClosedPipe;

    impl io::Write for ClosedPipe {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::ErrorKind::BrokenPipe.into())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn broken_pipe_is_recognised_through_serde_json() {
        let json = serde_json::to_writer(ClosedPipe, &serde_json::json!({"a": 1})).unwrap_err();
        assert!(is_broken_pipe(&json.into()));

        let mut pipe = ClosedPipe;
        let plain = io::Write::write_all(&mut pipe, b"x").unwrap_err();
        assert!(is_broken_pipe(&plain.into()));

        let other = io::Error::other("disk full");
        assert!(!is_broken_pipe(&other.into()));
        let syntax = serde_json::from_str::<serde_json::Value>("{").unwrap_err();
        assert!(!is_broken_pipe(&syntax.into()));
    }
}
