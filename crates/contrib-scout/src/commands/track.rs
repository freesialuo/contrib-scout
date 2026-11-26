//! `track add|rm|list`: the scope refreshed by scheduled sync (§9, §10). Step 5 owns this file.

use super::Context;
use crate::render::Report;
use anyhow::Result;
use clap::Subcommand;
use contrib_scout_core::{not_implemented, RepoName};

#[derive(Subcommand, Debug)]
pub enum TrackCommand {
    /// Track a repository.
    Add {
        /// owner/repo
        repo: RepoName,
    },
    /// Stop tracking a repository.
    Rm {
        /// owner/repo
        repo: RepoName,
    },
    /// List tracked repositories.
    List,
}

pub fn run(_ctx: &mut Context<'_>, command: &TrackCommand) -> Result<Report> {
    match command {
        TrackCommand::Add { .. } => Err(not_implemented("track add")),
        TrackCommand::Rm { .. } => Err(not_implemented("track rm")),
        TrackCommand::List => Err(not_implemented("track list")),
    }
}
