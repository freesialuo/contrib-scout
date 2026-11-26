//! Global options and the top-level command tree (§10). Each command group's
//! own arguments live next to its handler in `commands/<group>.rs`, so later
//! steps can change one command without touching this file.

use crate::commands::{
    cache::CacheCommand, candidates::CandidatesArgs, claim::ClaimCommand, config::ConfigCommand,
    discover::DiscoverArgs, gate::GateCommand, issue::IssueCommand, policy::PolicyCommand,
    repo::RepoCommand, sync::SyncArgs, track::TrackCommand,
};
use clap::{ArgAction, Parser, Subcommand};
use contrib_scout_core::{config, AgentId, Freshness};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "contrib-scout",
    version,
    about = "Find open-source issues that accept outside contributions, sharing one cache, quota, and claim table across agents",
    after_help = "Exit codes: 0 ok/pass, 10 changes or new candidates, 20 gate blocked, 1 error, 2 usage."
)]
pub struct Cli {
    /// Write one contrib-scout/v1 JSON envelope.
    #[arg(global = true, long, conflicts_with = "ndjson")]
    pub json: bool,

    /// Write one contrib-scout/v1 JSON envelope per output record.
    #[arg(global = true, long)]
    pub ndjson: bool,

    /// Override the shared SQLite database path.
    #[arg(global = true, long, env = config::ENV_DB, value_name = "PATH")]
    pub db: Option<PathBuf>,

    /// Override the TOML configuration path.
    #[arg(global = true, long, env = config::ENV_CONFIG, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Read only the local cache; never make requests. Gates refuse this mode.
    #[arg(global = true, long)]
    pub offline: bool,

    /// Refresh the requested data regardless of TTL.
    #[arg(global = true, long, conflicts_with = "offline")]
    pub fresh: bool,

    /// Cap requests for this invocation, including retries.
    #[arg(global = true, long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..))]
    pub max_requests: Option<u32>,

    /// Show request metrics on stderr.
    #[arg(global = true, short = 'v', long, action = ArgAction::Count)]
    pub verbose: u8,

    /// Agent id for claims, history, and request accounting.
    #[arg(global = true, long, env = config::ENV_AGENT, value_name = "ID")]
    pub agent: Option<AgentId>,

    /// Disable ANSI color in human output.
    #[arg(global = true, long)]
    pub no_color: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[allow(dead_code)] // used through `Context::freshness` by later steps
impl Cli {
    /// Read mode for non-gate commands. Gates always use `Freshness::Live`.
    pub fn freshness(&self) -> Freshness {
        if self.offline {
            Freshness::Offline
        } else if self.fresh {
            Freshness::Fresh
        } else {
            Freshness::Default
        }
    }
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Search for repositories and issues, cheapest checks first.
    Discover(DiscoverArgs),
    /// Repository metadata, activity, and policy mark.
    #[command(subcommand)]
    Repo(RepoCommand),
    /// Read policy text and record or inspect AI policy marks.
    #[command(subcommand)]
    Policy(PolicyCommand),
    /// Rank indexed issues from local data only (zero requests).
    Candidates(CandidatesArgs),
    /// Inspect an issue.
    #[command(subcommand)]
    Issue(IssueCommand),
    /// Live checks before starting work or opening a PR.
    #[command(subcommand)]
    Gate(GateCommand),
    /// Local claim leases on issues.
    #[command(subcommand)]
    Claim(ClaimCommand),
    /// Repositories refreshed by scheduled sync.
    #[command(subcommand)]
    Track(TrackCommand),
    /// Refresh the issue index.
    Sync(SyncArgs),
    /// Shared quota and window usage across all agents.
    Quota,
    /// Inspect or prune the local cache.
    #[command(subcommand)]
    Cache(CacheCommand),
    /// Read or change configuration.
    #[command(subcommand)]
    Config(ConfigCommand),
}
