//! Incremental issue index (§1, §6, §7). Step 5 owns this file.
//!
//! Sync uses `since` + `sort=updated` with an overlap window (config
//! `index_overlap`); only index fields are stored, bodies are fetched on demand.

use crate::config::Config;
use crate::error::{not_implemented, Result};
use crate::freshness::{Freshness, Live, LiveRead};
use crate::gateway::Gateway;
use crate::models::{IssueDetail, IssueRef, RepoName};
use crate::store::SharedStore;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncReport {
    pub repo: RepoName,
    /// Issues written (new or updated).
    pub updated: u32,
    /// Issues seen for the first time — drives exit code 10.
    pub new_issues: u32,
    pub since: Option<DateTime<Utc>>,
    /// Budget or quota stopped the sync early; the watermark was not advanced past what was stored.
    pub partial: bool,
}

pub trait IssueIndexer {
    /// Refreshes one repository's index according to `mode` (TTL 1h by default).
    fn sync(&self, repo: &RepoName, mode: Freshness) -> Result<SyncReport>;

    /// Issue with body, recent comments, and timeline PRs (TTL 30m, `issue show`).
    fn issue(&self, issue: &IssueRef, mode: Freshness) -> Result<IssueDetail>;

    /// Gate variant: always live (§8).
    fn issue_live(&self, issue: &IssueRef, live: LiveRead) -> Result<Live<IssueDetail>>;
}

pub struct GitHubIndexer {
    pub gateway: Arc<Gateway>,
    pub store: SharedStore,
    pub config: Config,
}

impl IssueIndexer for GitHubIndexer {
    fn sync(&self, _repo: &RepoName, _mode: Freshness) -> Result<SyncReport> {
        Err(not_implemented("issue sync"))
    }

    fn issue(&self, _issue: &IssueRef, _mode: Freshness) -> Result<IssueDetail> {
        Err(not_implemented("issue details"))
    }

    fn issue_live(&self, _issue: &IssueRef, _live: LiveRead) -> Result<Live<IssueDetail>> {
        Err(not_implemented("issue details"))
    }
}
