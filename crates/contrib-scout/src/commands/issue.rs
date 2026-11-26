//! `issue show` (§10). Step 5 owns this file.

use super::Context;
use crate::render::Report;
use anyhow::Result;
use clap::Subcommand;
use contrib_scout_core::{not_implemented, IssueRef};

#[derive(Subcommand, Debug)]
pub enum IssueCommand {
    /// Issue body, recent comments, and linked PRs (use --fresh to ignore the TTL).
    Show {
        /// owner/repo#N or an issue URL
        #[arg(value_name = "REF")]
        reference: IssueRef,
    },
}

pub fn run(_ctx: &mut Context<'_>, command: &IssueCommand) -> Result<Report> {
    match command {
        IssueCommand::Show { .. } => Err(not_implemented("issue show")),
    }
}
