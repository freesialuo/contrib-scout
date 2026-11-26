//! `repos`, `repo_activity`, `tracked_repos` (§6, §9). Steps 4 (profiles) and 5 (tracking) use this file.

use super::{Cached, Store};
use crate::error::{not_implemented, Result};
use crate::models::{AgentId, RepoActivity, RepoName, RepoProfile};
use chrono::{DateTime, Utc};
use serde_json::Value;

impl Store {
    /// Cached profile (without activity) and its ETag.
    pub fn repo(&self, _repo: &RepoName) -> Result<Option<(Cached<RepoProfile>, Option<String>)>> {
        Err(not_implemented("repository storage"))
    }

    /// Upserts metadata. `raw` is GitHub's JSON, kept for later fields.
    pub fn save_repo(
        &mut self,
        _profile: &RepoProfile,
        _raw: &Value,
        _etag: Option<&str>,
    ) -> Result<()> {
        Err(not_implemented("repository storage"))
    }

    /// Marks a cached copy as revalidated by a 304 at `at`.
    pub fn touch_repo(&mut self, _repo: &RepoName, _at: DateTime<Utc>) -> Result<()> {
        Err(not_implemented("repository storage"))
    }

    pub fn activity(&self, _repo: &RepoName) -> Result<Option<RepoActivity>> {
        Err(not_implemented("repository activity storage"))
    }

    pub fn save_activity(&mut self, _repo: &RepoName, _activity: &RepoActivity) -> Result<()> {
        Err(not_implemented("repository activity storage"))
    }

    /// Adds to the scheduled-sync set; returns false if already tracked.
    pub fn track(
        &mut self,
        _repo: &RepoName,
        _by: Option<&AgentId>,
        _at: DateTime<Utc>,
    ) -> Result<bool> {
        Err(not_implemented("tracked repositories"))
    }

    /// Returns false if it was not tracked.
    pub fn untrack(&mut self, _repo: &RepoName) -> Result<bool> {
        Err(not_implemented("tracked repositories"))
    }

    pub fn tracked(&self) -> Result<Vec<RepoName>> {
        Err(not_implemented("tracked repositories"))
    }
}
