//! `claims`, `gate_log` (§6, §8, §9). Step 6 owns this file.
//!
//! The partial unique index `claims_one_active` guarantees at most one active
//! claim per issue across processes; inserts must run in an IMMEDIATE transaction
//! that first expires stale leases.

use super::Store;
use crate::error::{not_implemented, Result};
use crate::models::{AgentId, Claim, ClaimOutcome, GateReport, IssueRef};
use chrono::{DateTime, Utc};

impl Store {
    /// Expires stale leases, then acquires, renews, or reports the holder.
    pub fn claim_issue(
        &mut self,
        _issue: &IssueRef,
        _agent: &AgentId,
        _now: DateTime<Utc>,
        _lease_until: DateTime<Utc>,
        _branch: Option<&str>,
    ) -> Result<ClaimOutcome> {
        Err(not_implemented("claim storage"))
    }

    /// Releases `agent`'s active claim. Returns false if it held none.
    pub fn release_claim(
        &mut self,
        _issue: &IssueRef,
        _agent: &AgentId,
        _now: DateTime<Utc>,
    ) -> Result<bool> {
        Err(not_implemented("claim storage"))
    }

    /// The unexpired active claim on `issue`, if any.
    pub fn active_claim(&self, _issue: &IssueRef, _now: DateTime<Utc>) -> Result<Option<Claim>> {
        Err(not_implemented("claim storage"))
    }

    /// Active claims, optionally for one agent.
    pub fn claims(&self, _agent: Option<&AgentId>, _now: DateTime<Utc>) -> Result<Vec<Claim>> {
        Err(not_implemented("claim storage"))
    }

    /// Marks leases past `lease_until` as expired; returns how many.
    pub fn expire_claims(&mut self, _now: DateTime<Utc>) -> Result<usize> {
        Err(not_implemented("claim storage"))
    }

    /// Appends a gate result with its evidence snapshot.
    pub fn log_gate(&mut self, _report: &GateReport) -> Result<()> {
        Err(not_implemented("gate log"))
    }
}
