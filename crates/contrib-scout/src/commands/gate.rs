//! `gate start|pr`: always live, fail closed (§8, §10). Step 6 owns this file.
//! Exit 0 pass, 20 blocked, 1 error. The agent id comes from the global `--agent`.

use super::Context;
use crate::render::Report;
use anyhow::Result;
use clap::Subcommand;
use contrib_scout_core::{not_implemented, IssueRef};

#[derive(Subcommand, Debug)]
pub enum GateCommand {
    /// Live checks before starting work; acquires or renews the claim.
    Start {
        /// owner/repo#N or an issue URL
        #[arg(value_name = "REF")]
        reference: IssueRef,
    },
    /// Live checks before opening a PR; returns full policy text and PR requirements.
    Pr {
        /// owner/repo#N or an issue URL
        #[arg(value_name = "REF")]
        reference: IssueRef,
        /// Your PR head branch, excluded from the linked-PR check.
        #[arg(long, value_name = "B")]
        branch: Option<String>,
    },
}

pub fn run(_ctx: &mut Context<'_>, command: &GateCommand) -> Result<Report> {
    match command {
        GateCommand::Start { .. } => Err(not_implemented("gate start")),
        GateCommand::Pr { .. } => Err(not_implemented("gate pr")),
    }
}
