//! Claim service over the [`Coordinator`] (§9). Step 6 owns this file.
//! Claims are local only — never written to GitHub. Default lease 48h; a repeat
//! `gate start` or `claim renew` extends it; expired leases are released.

use crate::coordinator::Coordinator;
use crate::error::{not_implemented, Result};
use crate::models::{AgentId, Claim, ClaimOutcome, IssueRef};
use crate::store::SharedStore;
use std::sync::Arc;
use std::time::Duration;

pub struct ClaimService {
    pub coordinator: Arc<dyn Coordinator>,
    pub store: SharedStore,
    pub lease: Duration,
}

impl ClaimService {
    /// Acquire or renew (used by `gate start`).
    pub fn acquire(&self, _issue: &IssueRef, _agent: &AgentId) -> Result<ClaimOutcome> {
        Err(not_implemented("claims"))
    }

    /// `claim renew`: fails with `ScoutError::NotFound` if `agent` holds no active claim.
    pub fn renew(&self, _issue: &IssueRef, _agent: &AgentId) -> Result<Claim> {
        Err(not_implemented("claims"))
    }

    /// `claim release`.
    pub fn release(&self, _issue: &IssueRef, _agent: &AgentId) -> Result<()> {
        Err(not_implemented("claims"))
    }

    /// `claim list`: active claims, optionally for one agent.
    pub fn list(&self, _agent: Option<&AgentId>) -> Result<Vec<Claim>> {
        Err(not_implemented("claims"))
    }

    /// `gate pr`: the unexpired claim held by `agent`, if any.
    pub fn held_by(&self, _issue: &IssueRef, _agent: &AgentId) -> Result<Option<Claim>> {
        Err(not_implemented("claims"))
    }
}
