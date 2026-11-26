//! `repo show` (§10). Step 4 owns this file.

use super::Context;
use crate::render::Report;
use anyhow::Result;
use clap::Subcommand;
use contrib_scout_core::{not_implemented, RepoName};

#[derive(Subcommand, Debug)]
pub enum RepoCommand {
    /// Metadata, activity, and policy mark (use --fresh to ignore the TTL).
    Show {
        /// owner/repo
        repo: RepoName,
    },
}

pub fn run(_ctx: &mut Context<'_>, command: &RepoCommand) -> Result<Report> {
    match command {
        RepoCommand::Show { .. } => Err(not_implemented("repo show")),
    }
}
