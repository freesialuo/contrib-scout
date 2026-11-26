mod args;
mod commands;
mod render;

use clap::Parser;
use std::{io, process::ExitCode};

fn main() -> ExitCode {
    // clap exits with 2 on usage errors and 0 for --help / --version.
    let cli = args::Cli::parse();
    match commands::run(&cli) {
        Ok(report) => match render::write(&report, &cli) {
            Ok(()) => ExitCode::from(report.exit.code()),
            Err(error) => fail(&error, &cli),
        },
        Err(error) => fail(&error, &cli),
    }
}

fn fail(error: &anyhow::Error, cli: &args::Cli) -> ExitCode {
    if error
        .downcast_ref::<io::Error>()
        .is_some_and(|e| e.kind() == io::ErrorKind::BrokenPipe)
    {
        return ExitCode::SUCCESS;
    }
    ExitCode::from(render::write_error(error, cli.json || cli.ndjson))
}
