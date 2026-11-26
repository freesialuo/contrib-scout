//! Mark storage service (§5). Step 3 owns this file.

use super::PolicyMarks;
use crate::config::Config;
use crate::error::{not_implemented, Result};
use crate::models::{AgentId, MarkTarget, PolicyMark, PolicyMarkEvent, RepoName, Stance};
use crate::store::SharedStore;
use chrono::{DateTime, TimeDelta, Utc};

/// `marked_at + TTL(stance)` (§5: Bans 90d; Restricts/Disclose 30d; others 14d).
pub fn expires_at(config: &Config, stance: &Stance, marked_at: DateTime<Utc>) -> DateTime<Utc> {
    let ttl = TimeDelta::from_std(config.mark_ttl(stance)).unwrap_or(TimeDelta::MAX);
    marked_at
        .checked_add_signed(ttl)
        .unwrap_or(DateTime::<Utc>::MAX_UTC)
}

pub struct StoreMarks {
    pub store: SharedStore,
    pub config: Config,
    pub now: DateTime<Utc>,
}

impl PolicyMarks for StoreMarks {
    fn get(&self, _repo: &RepoName) -> Result<Option<PolicyMark>> {
        Err(not_implemented("policy marks"))
    }

    fn get_target(&self, _target: &MarkTarget) -> Result<Option<PolicyMark>> {
        Err(not_implemented("policy marks"))
    }

    fn set(&self, _mark: &PolicyMark) -> Result<()> {
        Err(not_implemented("policy marks"))
    }

    fn unset(&self, _target: &MarkTarget, _agent: &AgentId) -> Result<Option<PolicyMark>> {
        Err(not_implemented("policy marks"))
    }

    fn history(&self, _target: &MarkTarget) -> Result<Vec<PolicyMarkEvent>> {
        Err(not_implemented("policy marks"))
    }

    fn due(&self, _now: DateTime<Utc>) -> Result<Vec<PolicyMark>> {
        Err(not_implemented("policy marks"))
    }
}
