//! `issues`, `sync_state` (§6, §7). Step 5 owns this file.

use super::{Cached, Store};
use crate::error::{not_implemented, Result};
use crate::models::{IssueRef, IssueRow, RepoName};
use chrono::{DateTime, Utc};
use serde_json::Value;

/// Incremental sync watermark for one repository.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SyncState {
    /// `since` for the next `sort=updated` pass (before subtracting the overlap).
    pub since: Option<DateTime<Utc>>,
    pub last_full_at: Option<DateTime<Utc>>,
    pub last_sync_at: Option<DateTime<Utc>>,
}

/// Local filter for `candidates` (zero requests).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IssueFilter {
    pub repos: Vec<RepoName>,
    pub language: Option<String>,
    pub labels: Vec<String>,
    pub open_only: bool,
    pub unassigned_only: bool,
    pub limit: Option<u32>,
}

impl Store {
    pub fn issue(&self, _issue: &IssueRef) -> Result<Option<Cached<IssueRow>>> {
        Err(not_implemented("issue index storage"))
    }

    /// Upserts index fields; `raw` is GitHub's JSON for the issue.
    pub fn save_issues(&mut self, _rows: &[(IssueRow, Value)]) -> Result<()> {
        Err(not_implemented("issue index storage"))
    }

    pub fn query_issues(&self, _filter: &IssueFilter) -> Result<Vec<IssueRow>> {
        Err(not_implemented("issue index storage"))
    }

    pub fn sync_state(&self, _repo: &RepoName) -> Result<SyncState> {
        Err(not_implemented("sync state"))
    }

    pub fn save_sync_state(&mut self, _repo: &RepoName, _state: &SyncState) -> Result<()> {
        Err(not_implemented("sync state"))
    }
}
