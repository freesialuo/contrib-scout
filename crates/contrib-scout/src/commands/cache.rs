//! `cache stats|prune` (§10). Step 2 owns this file.

use super::Context;
use crate::render::Report;
use anyhow::Result;
use clap::Subcommand;
use contrib_scout_core::config::parse_duration;
use contrib_scout_core::not_implemented;
use std::time::Duration;

#[derive(Subcommand, Debug)]
pub enum CacheCommand {
    /// Row counts and database size.
    Stats,
    /// Delete cached data older than a duration (never policy marks or history).
    Prune {
        /// Age such as 30d or 12h.
        #[arg(long, value_name = "D", default_value = "30d", value_parser = parse_age)]
        older_than: Duration,
    },
}

fn parse_age(value: &str) -> Result<Duration, String> {
    parse_duration(value).map_err(|e| format!("{e:#}"))
}

pub fn run(_ctx: &mut Context<'_>, command: &CacheCommand) -> Result<Report> {
    match command {
        CacheCommand::Stats => Err(not_implemented("cache stats")),
        CacheCommand::Prune { .. } => Err(not_implemented("cache prune")),
    }
}
