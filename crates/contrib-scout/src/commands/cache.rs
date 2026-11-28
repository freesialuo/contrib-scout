//! `cache stats|prune` (§10). Policy marks, their history, active claims and
//! tracked repositories are never pruned (see `Store::prune`); the gate log
//! only with `--gate-log-older-than`.

use super::Context;
use crate::render::Report;
use anyhow::Result;
use chrono::{DateTime, TimeDelta, Utc};
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
        /// Also delete gate log entries (audit records of past gates, each with
        /// the whole report) older than this age. Without it the gate log is kept.
        #[arg(long, value_name = "D", value_parser = parse_age)]
        gate_log_older_than: Option<Duration>,
    },
}

fn parse_age(value: &str) -> Result<Duration, String> {
    parse_duration(value).map_err(|e| format!("{e:#}"))
}

pub fn run(ctx: &mut Context<'_>, command: &CacheCommand) -> Result<Report> {
    match command {
        CacheCommand::Stats => stats(ctx),
        CacheCommand::Prune {
            older_than,
            gate_log_older_than,
        } => prune(ctx, *older_than, *gate_log_older_than),
    }
}

/// `now - age`, or a usage error naming `flag` when that is out of range.
fn cutoff_for(now: DateTime<Utc>, age: Duration, flag: &str) -> Result<DateTime<Utc>> {
    let too_large = || ScoutError::Usage(format!("{flag} is too large"));
    let age = TimeDelta::from_std(age).map_err(|_| too_large())?;
    Ok(now.checked_sub_signed(age).ok_or_else(too_large)?)
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
    human.push(format!(
        "Gate log {} row(s), {} bytes of stored reports (prune with --gate-log-older-than)",
        stats.gate_log.rows, stats.gate_log.bytes
    ));
    let data = json!({
        "path": path,
        "rows": stats.rows,
        "bytes": stats.bytes,
        "gate_log": stats.gate_log,
    });
    Ok(Report::new(data).human(human))
}

fn prune(
    ctx: &mut Context<'_>,
    older_than: Duration,
    gate_log_older_than: Option<Duration>,
) -> Result<Report> {
    let cutoff = cutoff_for(ctx.now, older_than, "--older-than")?;
    let gate_log_cutoff = gate_log_older_than
        .map(|age| cutoff_for(ctx.now, age, "--gate-log-older-than"))
        .transpose()?;
    let mut store = ctx.open_store()?;
    let mut pruned = store.prune(cutoff)?;
    if let Some(gate_cutoff) = gate_log_cutoff {
        let rows = store.prune_gate_log(gate_cutoff)?;
        pruned.deleted.insert("gate_log".into(), rows);
    }
    let total: u64 = pruned.deleted.values().sum();
    let mut human = vec![format!(
        "Deleted {total} row(s) fetched before {}",
        cutoff.to_rfc3339()
    )];
    if let Some(gate_cutoff) = gate_log_cutoff {
        human[0].push_str(&format!(
            " (gate log: recorded before {})",
            gate_cutoff.to_rfc3339()
        ));
    }
    for (table, rows) in pruned.deleted.iter().filter(|(_, rows)| **rows > 0) {
        human.push(format!("  {table:<22} {rows:>8}"));
    }
    let data = json!({
        "cutoff": cutoff,
        "deleted": pruned.deleted,
        "total": total,
        "gate_log_cutoff": gate_log_cutoff,
    });
    Ok(Report::new(data).human(human))
}
