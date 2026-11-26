//! `policy_marks`, `policy_mark_history`, `policy_files` (§5, §6). Step 3 owns this file.

use super::{Cached, Store};
use crate::error::{not_implemented, Result};
use crate::models::{AgentId, MarkTarget, PolicyFile, PolicyMark, PolicyMarkEvent, RepoName};
use chrono::{DateTime, Utc};

/// A cached candidate-path probe. `file` is `None` when GitHub answered 404,
/// so absence is cached too and revalidated with its ETag.
#[derive(Clone, Debug, PartialEq)]
pub struct CachedPolicyFile {
    pub repo: RepoName,
    pub path: String,
    pub status: u16,
    pub file: Option<PolicyFile>,
    pub etag: Option<String>,
    pub fetched_at: DateTime<Utc>,
}

impl Store {
    /// The mark stored for exactly this target.
    pub fn mark(&self, _target: &MarkTarget) -> Result<Option<PolicyMark>> {
        Err(not_implemented("policy mark storage"))
    }

    /// The mark that applies to `repo`: its own, else its org's (§5 step 1).
    pub fn effective_mark(&self, _repo: &RepoName) -> Result<Option<PolicyMark>> {
        Err(not_implemented("policy mark storage"))
    }

    /// Upserts the mark and appends a `set` history row in one transaction.
    /// Callers validate first ([`PolicyMark::validate`]).
    pub fn put_mark(&mut self, _mark: &PolicyMark) -> Result<()> {
        Err(not_implemented("policy mark storage"))
    }

    /// Deletes the mark and appends an `unset` history row holding the removed
    /// mark. Returns the removed mark, if any.
    pub fn delete_mark(
        &mut self,
        _target: &MarkTarget,
        _agent: &AgentId,
        _at: DateTime<Utc>,
    ) -> Result<Option<PolicyMark>> {
        Err(not_implemented("policy mark storage"))
    }

    /// History for a target, oldest first.
    pub fn mark_history(&self, _target: &MarkTarget) -> Result<Vec<PolicyMarkEvent>> {
        Err(not_implemented("policy mark storage"))
    }

    /// Marks with `expires_at <= now` (`policy due`). Expired marks are never deleted.
    pub fn due_marks(&self, _now: DateTime<Utc>) -> Result<Vec<PolicyMark>> {
        Err(not_implemented("policy mark storage"))
    }

    /// Effective marks for many repos at once (discovery's zero-request policy step, §7).
    pub fn effective_marks(&self, _repos: &[RepoName]) -> Result<Vec<(RepoName, PolicyMark)>> {
        Err(not_implemented("policy mark storage"))
    }

    pub fn cached_policy_file(
        &self,
        _repo: &RepoName,
        _path: &str,
    ) -> Result<Option<Cached<CachedPolicyFile>>> {
        Err(not_implemented("policy file cache"))
    }

    pub fn save_policy_file(&mut self, _file: &CachedPolicyFile) -> Result<()> {
        Err(not_implemented("policy file cache"))
    }
}
