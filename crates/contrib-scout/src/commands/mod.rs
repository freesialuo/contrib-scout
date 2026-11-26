//! Command handlers, one file per command group. Each file owns its clap
//! arguments and a `run(ctx, …) -> Result<Report>` handler.
//!
//! Handlers must fill `report.meta.reminders` for every repository they touch
//! (§5) and set `report.exit` for changes (10) or blocked gates (20).

pub mod cache;
pub mod candidates;
pub mod claim;
pub mod config;
pub mod discover;
pub mod gate;
pub mod issue;
pub mod policy;
pub mod quota;
pub mod repo;
pub mod sync;
pub mod track;

use crate::args::{Cli, Command};
use crate::render::Report;
use anyhow::Result;
use chrono::{DateTime, Utc};
use contrib_scout_core::config::{default_config_path, default_db_path, Config, ConfigFile};
use contrib_scout_core::store::Store;
use contrib_scout_core::{AgentId, Freshness, ScoutError};
use std::path::PathBuf;

/// Per-invocation context. Everything is resolved lazily so stubs and
/// `config` never open the database.
pub struct Context<'a> {
    pub cli: &'a Cli,
    pub now: DateTime<Utc>,
}

#[allow(dead_code)] // helpers for the handlers filled in by later steps
impl Context<'_> {
    pub fn freshness(&self) -> Freshness {
        self.cli.freshness()
    }

    pub fn config_file(&self) -> Result<ConfigFile> {
        let path = match &self.cli.config {
            Some(path) => path.clone(),
            None => default_config_path()?,
        };
        Ok(ConfigFile::new(path))
    }

    pub fn config(&self) -> Result<Config> {
        self.config_file()?.load()
    }

    pub fn db_path(&self) -> Result<PathBuf> {
        match &self.cli.db {
            Some(path) => Ok(path.clone()),
            None => default_db_path(),
        }
    }

    pub fn open_store(&self) -> Result<Store> {
        Store::open(self.db_path()?)
    }

    /// `--agent` / `CONTRIB_SCOUT_AGENT`. Required by commands that write
    /// claims or mark history.
    pub fn agent(&self) -> Result<AgentId> {
        self.cli.agent.clone().ok_or_else(|| {
            ScoutError::Usage(
                "an agent id is required; pass --agent ID or set CONTRIB_SCOUT_AGENT".into(),
            )
            .into()
        })
    }

    /// Request cap: `--max-requests`, else config `max_requests`.
    pub fn max_requests(&self, config: &Config) -> u32 {
        self.cli.max_requests.unwrap_or(config.max_requests)
    }
}

pub fn run(cli: &Cli) -> Result<Report> {
    let mut ctx = Context {
        cli,
        now: Utc::now(),
    };
    let mut report = match &cli.command {
        Command::Discover(args) => discover::run(&mut ctx, args),
        Command::Repo(command) => repo::run(&mut ctx, command),
        Command::Policy(command) => policy::run(&mut ctx, command),
        Command::Candidates(args) => candidates::run(&mut ctx, args),
        Command::Issue(command) => issue::run(&mut ctx, command),
        Command::Gate(command) => gate::run(&mut ctx, command),
        Command::Claim(command) => claim::run(&mut ctx, command),
        Command::Track(command) => track::run(&mut ctx, command),
        Command::Sync(args) => sync::run(&mut ctx, args),
        Command::Quota => quota::run(&mut ctx),
        Command::Cache(command) => cache::run(&mut ctx, command),
        Command::Config(command) => config::run(&mut ctx, command),
    }?;
    if report.meta.as_of.is_none() {
        report.meta.as_of = Some(ctx.now);
    }
    Ok(report)
}
