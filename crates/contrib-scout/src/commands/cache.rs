//! `cache stats|prune` (§10). Policy marks, their history, active claims and
//! tracked repositories are never pruned (see `Store::prune`).

use super::Context;
use crate::render::Report;
use anyhow::Result;
use chrono::TimeDelta;
use clap::Subcommand;
use contrib_scout_core::config::parse_duration;
use contrib_scout_core::ScoutError;
use serde_json::json;
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

pub fn run(ctx: &mut Context<'_>, command: &CacheCommand) -> Result<Report> {
    match command {
        CacheCommand::Stats => stats(ctx),
        CacheCommand::Prune { older_than } => prune(ctx, *older_than),
    }
}

fn stats(ctx: &mut Context<'_>) -> Result<Report> {
    let path = ctx.db_path()?;
    let store = ctx.open_store()?;
    let stats = store.stats()?;
    let mut human = vec![format!("Database {}", path.display())];
    if let Some(bytes) = stats.bytes {
        human.push(format!("Size {bytes} bytes"));
    }
    for (table, rows) in &stats.rows {
        human.push(format!("  {table:<22} {rows:>8}"));
    }
    let data = json!({ "path": path, "rows": stats.rows, "bytes": stats.bytes });
    Ok(Report::new(data).human(human))
}

fn prune(ctx: &mut Context<'_>, older_than: Duration) -> Result<Report> {
    let age = TimeDelta::from_std(older_than)
        .map_err(|_| ScoutError::Usage("--older-than is too large".into()))?;
    let cutoff = ctx
        .now
        .checked_sub_signed(age)
        .ok_or_else(|| ScoutError::Usage("--older-than is too large".into()))?;
    let mut store = ctx.open_store()?;
    let pruned = store.prune(cutoff)?;
    let total: u64 = pruned.deleted.values().sum();
    let mut human = vec![format!(
        "Deleted {total} row(s) fetched before {}",
        cutoff.to_rfc3339()
    )];
    for (table, rows) in pruned.deleted.iter().filter(|(_, rows)| **rows > 0) {
        human.push(format!("  {table:<22} {rows:>8}"));
    }
    let data = json!({ "cutoff": cutoff, "deleted": pruned.deleted, "total": total });
    Ok(Report::new(data).human(human))
}
