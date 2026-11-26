//! AI policy: raw files, agent-written marks, reminders (§5). The CLI never
//! decides whether a project accepts AI contributions; it serves the original
//! text, stores the agent's mark, and reminds every later reader of it.

pub mod files;
pub mod marks;
pub mod reminders;

pub use files::{AbsentPath, GitHubPolicyFiles, PolicyFetch, PolicySource, CANDIDATE_PATHS};
pub use marks::{expires_at, StoreMarks};
pub use reminders::{human_line, reminder_for, repo_reminder, repo_reminders};

use crate::error::Result;
use crate::freshness::{Freshness, Live, LiveRead};
use crate::models::{AgentId, MarkTarget, PolicyFile, PolicyMark, PolicyMarkEvent, RepoName};
use chrono::{DateTime, Utc};

/// Fetches original policy text only; no judgment (§4, §5 step 4).
pub trait PolicyFiles {
    /// Org `{owner}/.github` first, then the repo's candidate paths
    /// ([`CANDIDATE_PATHS`]), with conditional requests. Returns found files only.
    fn fetch(&self, repo: &RepoName, mode: Freshness) -> Result<Vec<PolicyFile>>;

    /// Gate variant (§8): always live; `with_text` is false for `gate start`
    /// (list only) and true for `gate pr` (full text).
    fn fetch_live(
        &self,
        repo: &RepoName,
        live: LiveRead,
        with_text: bool,
    ) -> Result<Live<Vec<PolicyFile>>>;
}

/// Agent-written marks, local only, zero requests (§4, §5).
pub trait PolicyMarks {
    /// Effective mark for a repository: its own, else its org's.
    fn get(&self, repo: &RepoName) -> Result<Option<PolicyMark>>;

    /// The mark stored for exactly this target (`policy show`).
    fn get_target(&self, target: &MarkTarget) -> Result<Option<PolicyMark>>;

    /// Validates format, writes or overwrites, and appends history.
    fn set(&self, mark: &PolicyMark) -> Result<()>;

    /// Removes the mark and appends history. Returns the removed mark.
    fn unset(&self, target: &MarkTarget, agent: &AgentId) -> Result<Option<PolicyMark>>;

    fn history(&self, target: &MarkTarget) -> Result<Vec<PolicyMarkEvent>>;

    /// Marks whose `expires_at` has passed (`policy due`).
    fn due(&self, now: DateTime<Utc>) -> Result<Vec<PolicyMark>>;
}
