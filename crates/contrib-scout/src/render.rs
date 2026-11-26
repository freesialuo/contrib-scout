//! Output: the versioned JSON envelope (`--json` / `--ndjson`) or human text.
//!
//! Commands build a [`Report`] with their own human lines, so this file does
//! not need per-command changes. Reminders are always printed prominently.

use crate::args::Cli;
use contrib_scout_core::envelope::{Envelope, ErrorInfo, Meta, Page};
use contrib_scout_core::policy::human_line;
use serde_json::Value;
use std::io::{self, IsTerminal, Write};

/// Non-error exit statuses (§10).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[allow(dead_code)] // `Changes` and `Blocked` are set by later steps
pub enum Exit {
    #[default]
    Ok,
    /// New changes or new candidates.
    Changes,
    /// A gate blocked.
    Blocked,
}

impl Exit {
    pub fn code(self) -> u8 {
        match self {
            Self::Ok => 0,
            Self::Changes => 10,
            Self::Blocked => 20,
        }
    }
}

pub struct Report {
    pub data: Value,
    pub meta: Meta,
    pub page: Option<Page>,
    /// Human-readable lines; when empty, `data` is printed as pretty JSON.
    pub human: Vec<String>,
    pub exit: Exit,
}

impl Report {
    pub fn new(data: Value) -> Self {
        Self {
            data,
            meta: Meta::default(),
            page: None,
            human: Vec::new(),
            exit: Exit::Ok,
        }
    }

    pub fn human(mut self, lines: Vec<String>) -> Self {
        self.human = lines;
        self
    }
}

pub fn write(report: &Report, cli: &Cli) -> anyhow::Result<()> {
    let mut stdout = io::stdout().lock();
    let mut stderr = io::stderr().lock();
    if cli.json || cli.ndjson {
        let records = report
            .data
            .as_array()
            .filter(|records| cli.ndjson && !records.is_empty());
        if let Some(records) = records {
            for record in records {
                machine(&mut stdout, report, record)?;
            }
        } else {
            machine(&mut stdout, report, &report.data)?;
        }
    } else {
        let color = stdout.is_terminal() && !cli.no_color && std::env::var_os("NO_COLOR").is_none();
        for reminder in &report.meta.reminders {
            let line = human_line(reminder);
            if color {
                writeln!(stdout, "\x1b[1;33m{line}\x1b[0m")?;
            } else {
                writeln!(stdout, "{line}")?;
            }
        }
        if report.human.is_empty() {
            writeln!(stdout, "{}", serde_json::to_string_pretty(&report.data)?)?;
        } else {
            for line in &report.human {
                writeln!(stdout, "{}", clean(line))?;
            }
        }
        if report.meta.partial {
            writeln!(stderr, "Partial results")?;
        }
        for warning in &report.meta.warnings {
            writeln!(stderr, "Warning: {}", clean(warning))?;
        }
    }
    if cli.verbose > 0 {
        writeln!(
            stderr,
            "Requests: {} ({} unchanged)",
            report.meta.requests, report.meta.served_304
        )?;
    }
    stdout.flush()?;
    Ok(())
}

fn machine(output: &mut impl Write, report: &Report, data: &Value) -> anyhow::Result<()> {
    let envelope = Envelope {
        page: report.page.clone(),
        ..Envelope::new(report.meta.clone(), data)
    };
    serde_json::to_writer(&mut *output, &envelope)?;
    writeln!(output)?;
    Ok(())
}

/// Renders a failure and returns its exit code. In JSON modes the error
/// envelope goes to stdout; the message always goes to stderr.
pub fn write_error(error: &anyhow::Error, json: bool) -> u8 {
    let info = ErrorInfo::from_error(error);
    if json {
        let meta = Meta {
            as_of: Some(chrono::Utc::now()),
            ..Meta::default()
        };
        let envelope = Envelope::failure(meta, error);
        if let Ok(text) = serde_json::to_string(&envelope) {
            let mut stdout = io::stdout().lock();
            let _ = writeln!(stdout, "{text}");
            let _ = stdout.flush();
        }
    }
    eprintln!("error: {}", clean(&info.message));
    info.exit_code
}

/// Strips control characters (except newline and tab) from untrusted text.
pub fn clean(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect()
}
