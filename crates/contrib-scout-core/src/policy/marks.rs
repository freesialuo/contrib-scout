//! Mark storage service (§5): agent-written marks, local only, zero requests.

use super::PolicyMarks;
use crate::config::Config;
use crate::error::{Result, ScoutError};
use crate::models::{AgentId, MarkTarget, PolicyMark, PolicyMarkEvent, RepoName, Stance};
use crate::store::{SharedStore, Store};
use chrono::{DateTime, TimeDelta, Utc};
use std::sync::MutexGuard;

/// `marked_at + TTL(stance)` (§5: Bans 90d; Restricts/Disclose 30d; others 14d).
pub fn expires_at(config: &Config, stance: &Stance, marked_at: DateTime<Utc>) -> DateTime<Utc> {
    let ttl = TimeDelta::from_std(config.mark_ttl(stance)).unwrap_or(TimeDelta::MAX);
    marked_at
        .checked_add_signed(ttl)
        .unwrap_or(DateTime::<Utc>::MAX_UTC)
}

/// [`PolicyMarks`] over the shared store. `now` stamps `unset` history rows.
pub struct StoreMarks {
    pub store: SharedStore,
    pub config: Config,
    pub now: DateTime<Utc>,
}

impl StoreMarks {
    pub fn new(store: SharedStore, config: Config, now: DateTime<Utc>) -> Self {
        Self { store, config, now }
    }

    fn store(&self) -> MutexGuard<'_, Store> {
        self.store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl PolicyMarks for StoreMarks {
    fn get(&self, repo: &RepoName) -> Result<Option<PolicyMark>> {
        self.store().effective_mark(repo)
    }

    fn get_target(&self, target: &MarkTarget) -> Result<Option<PolicyMark>> {
        self.store().mark(target)
    }

    /// Rejects malformed marks with a usage error before anything is written.
    fn set(&self, mark: &PolicyMark) -> Result<()> {
        mark.validate()
            .map_err(|error| ScoutError::Usage(error.to_string()))?;
        self.store().put_mark(mark)
    }

    fn unset(&self, target: &MarkTarget, agent: &AgentId) -> Result<Option<PolicyMark>> {
        self.store().delete_mark(target, agent, self.now)
    }

    fn history(&self, target: &MarkTarget) -> Result<Vec<PolicyMarkEvent>> {
        self.store().mark_history(target)
    }

    fn due(&self, now: DateTime<Utc>) -> Result<Vec<PolicyMark>> {
        self.store().due_marks(now)
    }
}
