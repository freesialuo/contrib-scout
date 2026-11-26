//! `claim list|renew|release` (§9, §10). Step 6 owns this file.

use super::Context;
use crate::render::Report;
use anyhow::Result;
use clap::Subcommand;
use contrib_scout_core::{not_implemented, IssueRef};

#[derive(Subcommand, Debug)]
pub enum ClaimCommand {
    /// List active claims.
    List,
    /// Extend your lease on an issue.
    Renew {
        /// owner/repo#N or an issue URL
        #[arg(value_name = "REF")]
        reference: IssueRef,
    },
    /// Release your claim on an issue.
    Release {
        /// owner/repo#N or an issue URL
        #[arg(value_name = "REF")]
        reference: IssueRef,
    },
}

pub fn run(_ctx: &mut Context<'_>, command: &ClaimCommand) -> Result<Report> {
    match command {
        ClaimCommand::List => Err(not_implemented("claim list")),
        ClaimCommand::Renew { .. } => Err(not_implemented("claim renew")),
        ClaimCommand::Release { .. } => Err(not_implemented("claim release")),
    }
}
