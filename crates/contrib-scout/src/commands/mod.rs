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
use contrib_scout_core::config::{
    default_config_path, default_db_path, Config, ConfigFile, TransportKind,
};
use contrib_scout_core::coordinator::SqliteCoordinator;
use contrib_scout_core::gateway::{Budget, Gateway, HostPolicy};
use contrib_scout_core::store::{SharedStore, Store};
use contrib_scout_core::transport::{GhTransport, HttpTransport, Transport};
use contrib_scout_core::{AgentId, Freshness, ScoutError};
use std::path::PathBuf;
use std::sync::Arc;

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

    /// Agent id for request logging and quota attribution: `--agent` /
    /// `CONTRIB_SCOUT_AGENT`, else `anonymous`. Commands that write claims or
    /// marks must use [`Context::agent`] instead, which requires an id.
    pub fn agent_or_anonymous(&self) -> AgentId {
        self.cli
            .agent
            .clone()
            .unwrap_or_else(|| "anonymous".parse().expect("valid agent id"))
    }

    /// Opens the shared store and builds the one outbound [`Gateway`] for this
    /// invocation: configured transport, shared-quota coordinator, response
    /// cache, per-invocation budget. Every command that talks to GitHub uses
    /// this; nothing else may build its own transport (design §3, §9).
    pub fn services(&self) -> Result<Services> {
        let config = self.config()?;
        let store = self.open_store()?.shared();
        let transport: Arc<dyn Transport> = match config.transport {
            TransportKind::Gh => Arc::new(GhTransport::new(&config.gh_path)),
            TransportKind::Http => Arc::new(HttpTransport::new(None).with_gh(&config.gh_path)),
        };
        let coordinator = Arc::new(SqliteCoordinator::from_config(
            store.clone(),
            self.agent_or_anonymous(),
            &config,
        ));
        let policy = HostPolicy {
            rate_per_second: config.api_rate,
            burst: config.api_burst,
            ..HostPolicy::default()
        };
        let gateway = Gateway::new(transport, policy, Budget::new(self.max_requests(&config)))
            .with_coordinator(coordinator)
            .with_cache(store.clone())
            .with_lock_wait(config.lock_wait());
        Ok(Services {
            config,
            store,
            gateway: Arc::new(gateway),
        })
    }
}

/// What a network-capable command needs. Copy `gateway.metrics()` into
/// `report.meta` (`requests`, `served_304`) before returning.
#[allow(dead_code)] // consumed by the handlers filled in by later steps
pub struct Services {
    pub config: Config,
    pub store: SharedStore,
    pub gateway: Arc<Gateway>,
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
