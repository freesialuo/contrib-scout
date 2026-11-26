//! Claim service over the [`Coordinator`] (§9).
//! Claims are local only — never written to GitHub. Default lease 48h
//! (`claim_lease`); a passing `gate start` or `claim renew` extends it; expired
//! leases are released automatically (marked `expired` on the next write).

use crate::coordinator::Coordinator;
use crate::error::{Result, ScoutError};
use crate::models::{AgentId, Claim, ClaimOutcome, IssueRef};
use crate::store::{SharedStore, Store};
use chrono::{DateTime, TimeDelta, Utc};
use std::sync::{Arc, MutexGuard, PoisonError};
use std::time::Duration;

pub struct ClaimService {
    pub coordinator: Arc<dyn Coordinator>,
    pub store: SharedStore,
    pub lease: Duration,
}

/// `now + lease`, saturating.
pub fn lease_end(now: DateTime<Utc>, lease: Duration) -> DateTime<Utc> {
    TimeDelta::from_std(lease)
        .ok()
        .and_then(|lease| now.checked_add_signed(lease))
        .unwrap_or(DateTime::<Utc>::MAX_UTC)
}

impl ClaimService {
    pub fn new(coordinator: Arc<dyn Coordinator>, store: SharedStore, lease: Duration) -> Self {
        Self {
            coordinator,
            store,
            lease,
        }
    }

    fn store(&self) -> MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Acquire or renew (used by `gate start`). Atomic across processes.
    pub fn acquire(&self, issue: &IssueRef, agent: &AgentId) -> Result<ClaimOutcome> {
        self.coordinator.claim(issue, agent, self.lease)
    }

    /// `claim renew`: fails with `ScoutError::NotFound` if `agent` holds no active claim.
    pub fn renew(&self, issue: &IssueRef, agent: &AgentId) -> Result<Claim> {
        self.extend(issue, agent, None)
    }

    /// `gate pr --branch`: renews the held claim and records the PR head branch.
    pub fn record_branch(&self, issue: &IssueRef, agent: &AgentId, branch: &str) -> Result<Claim> {
        self.extend(issue, agent, Some(branch))
    }

    fn extend(&self, issue: &IssueRef, agent: &AgentId, branch: Option<&str>) -> Result<Claim> {
        let now = Utc::now();
        let mut store = self.store();
        let held = store
            .active_claim(issue, now)?
            .filter(|claim| &claim.agent == agent);
        if held.is_none() {
            return Err(not_held(issue, agent));
        }
        match store.claim_issue(issue, agent, now, lease_end(now, self.lease), branch)? {
            ClaimOutcome::Renewed { claim } | ClaimOutcome::Acquired { claim } => Ok(claim),
            ClaimOutcome::HeldByOther { .. } => Err(not_held(issue, agent)),
        }
    }

    /// `claim release`. Returns false when `agent` held no active claim.
    pub fn release(&self, issue: &IssueRef, agent: &AgentId) -> Result<bool> {
        self.store().release_claim(issue, agent, Utc::now())
    }

    /// `claim list`: active claims, optionally for one agent.
    pub fn list(&self, agent: Option<&AgentId>) -> Result<Vec<Claim>> {
        self.store().claims(agent, Utc::now())
    }

    /// The unexpired claim on `issue`, whoever holds it.
    pub fn active(&self, issue: &IssueRef) -> Result<Option<Claim>> {
        self.store().active_claim(issue, Utc::now())
    }

    /// `gate pr`: the unexpired claim held by `agent`, if any.
    pub fn held_by(&self, issue: &IssueRef, agent: &AgentId) -> Result<Option<Claim>> {
        Ok(self.active(issue)?.filter(|claim| &claim.agent == agent))
    }
}

fn not_held(issue: &IssueRef, agent: &AgentId) -> anyhow::Error {
    ScoutError::NotFound {
        what: format!("an active claim on {issue} held by {agent}"),
    }
    .into()
}
