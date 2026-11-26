//! Repository profiles: metadata and contributability metrics (§1, §6, §7). Step 4 owns this file.

use crate::config::Config;
use crate::error::{not_implemented, Result};
use crate::freshness::{Freshness, Live, LiveRead};
use crate::gateway::Gateway;
use crate::models::{RepoActivity, RepoName, RepoProfile};
use crate::store::SharedStore;
use std::sync::Arc;

pub trait RepoProfiler {
    /// Metadata (TTL 7d, conditional request) plus cached activity if present.
    fn profile(&self, repo: &RepoName, mode: Freshness) -> Result<RepoProfile>;

    /// External PR merge rate, median first response, maintainer activity.
    /// Uses search and GraphQL — the most expensive data; compute only for
    /// repositories that passed the policy check (§7).
    fn activity(&self, repo: &RepoName, mode: Freshness) -> Result<RepoActivity>;

    /// Gate variant: live metadata (archived check, §8).
    fn profile_live(&self, repo: &RepoName, live: LiveRead) -> Result<Live<RepoProfile>>;
}

pub struct GitHubProfiler {
    pub gateway: Arc<Gateway>,
    pub store: SharedStore,
    pub config: Config,
}

impl RepoProfiler for GitHubProfiler {
    fn profile(&self, _repo: &RepoName, _mode: Freshness) -> Result<RepoProfile> {
        Err(not_implemented("repository profiles"))
    }

    fn activity(&self, _repo: &RepoName, _mode: Freshness) -> Result<RepoActivity> {
        Err(not_implemented("repository activity"))
    }

    fn profile_live(&self, _repo: &RepoName, _live: LiveRead) -> Result<Live<RepoProfile>> {
        Err(not_implemented("repository profiles"))
    }
}
