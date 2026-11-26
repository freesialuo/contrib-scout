//! `claims`, `gate_log` (§6, §8, §9). Step 6 owns this file.
//!
//! The partial unique index `claims_one_active` guarantees at most one active
//! claim per issue across processes; inserts must run in an IMMEDIATE transaction
//! that first expires stale leases.

use super::{from_unix, Store};
use crate::error::Result;
use crate::models::{AgentId, Claim, ClaimOutcome, ClaimStatus, GateReport, IssueRef};
use anyhow::Context;
use chrono::{DateTime, Utc};
use rusqlite::{params, OptionalExtension, Row, TransactionBehavior};

const CLAIM_COLUMNS: &str = "issue_ref, agent_id, claimed_at, lease_until, status, branch";

fn claim_from_row(row: &Row<'_>) -> rusqlite::Result<ClaimRow> {
    Ok(ClaimRow {
        issue: row.get(0)?,
        agent: row.get(1)?,
        claimed_at: row.get(2)?,
        lease_until: row.get(3)?,
        status: row.get(4)?,
        branch: row.get(5)?,
    })
}

/// Raw columns; converted outside rusqlite so parse errors keep their context.
struct ClaimRow {
    issue: String,
    agent: String,
    claimed_at: i64,
    lease_until: i64,
    status: String,
    branch: Option<String>,
}

impl ClaimRow {
    fn into_claim(self) -> Result<Claim> {
        Ok(Claim {
            issue: self
                .issue
                .parse()
                .with_context(|| format!("stored claim issue {:?}", self.issue))?,
            agent: self
                .agent
                .parse()
                .with_context(|| format!("stored claim agent {:?}", self.agent))?,
            claimed_at: from_unix(self.claimed_at)?,
            lease_until: from_unix(self.lease_until)?,
            status: ClaimStatus::from_stored(&self.status),
            branch: self.branch,
        })
    }
}

impl Store {
    /// Expires stale leases, then acquires, renews, or reports the holder.
    ///
    /// A lease is stale once `lease_until <= now`. Renewal sets the lease to
    /// `lease_until` and keeps the original `claimed_at`; `branch` replaces the
    /// stored one only when given.
    pub fn claim_issue(
        &mut self,
        issue: &IssueRef,
        agent: &AgentId,
        now: DateTime<Utc>,
        lease_until: DateTime<Utc>,
        branch: Option<&str>,
    ) -> Result<ClaimOutcome> {
        let issue_ref = issue.to_string();
        let now_s = now.timestamp();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE claims SET status = 'expired', updated_at = ?1
             WHERE status = 'active' AND lease_until <= ?1",
            params![now_s],
        )?;
        let current = tx
            .query_row(
                &format!(
                    "SELECT {CLAIM_COLUMNS}, id FROM claims WHERE issue_ref = ?1 AND status = 'active'"
                ),
                params![issue_ref],
                |row| Ok((claim_from_row(row)?, row.get::<_, i64>(6)?)),
            )
            .optional()?;
        let outcome = match current {
            Some((row, id)) if row.agent == agent.as_str() => {
                tx.execute(
                    "UPDATE claims SET lease_until = ?2, branch = COALESCE(?3, branch), updated_at = ?4
                     WHERE id = ?1",
                    params![id, lease_until.timestamp(), branch, now_s],
                )?;
                let mut claim = row.into_claim()?;
                claim.lease_until = from_unix(lease_until.timestamp())?;
                if let Some(branch) = branch {
                    claim.branch = Some(branch.to_owned());
                }
                ClaimOutcome::Renewed { claim }
            }
            Some((row, _)) => ClaimOutcome::HeldByOther {
                claim: row.into_claim()?,
            },
            None => {
                tx.execute(
                    "INSERT INTO claims (issue_ref, agent_id, claimed_at, lease_until, status, branch, updated_at)
                     VALUES (?1, ?2, ?3, ?4, 'active', ?5, ?3)",
                    params![
                        issue_ref,
                        agent.as_str(),
                        now_s,
                        lease_until.timestamp(),
                        branch
                    ],
                )?;
                ClaimOutcome::Acquired {
                    claim: Claim {
                        issue: issue.clone(),
                        agent: agent.clone(),
                        claimed_at: from_unix(now_s)?,
                        lease_until: from_unix(lease_until.timestamp())?,
                        status: ClaimStatus::Active,
                        branch: branch.map(str::to_owned),
                    },
                }
            }
        };
        tx.commit()?;
        Ok(outcome)
    }

    /// Releases `agent`'s active claim. Returns false if it held none.
    /// An already expired lease counts as not held (it is marked expired).
    pub fn release_claim(
        &mut self,
        issue: &IssueRef,
        agent: &AgentId,
        now: DateTime<Utc>,
    ) -> Result<bool> {
        let now_s = now.timestamp();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE claims SET status = 'expired', updated_at = ?1
             WHERE status = 'active' AND lease_until <= ?1",
            params![now_s],
        )?;
        let changed = tx.execute(
            "UPDATE claims SET status = 'released', updated_at = ?3
             WHERE issue_ref = ?1 AND agent_id = ?2 AND status = 'active'",
            params![issue.to_string(), agent.as_str(), now_s],
        )?;
        tx.commit()?;
        Ok(changed > 0)
    }

    /// The unexpired active claim on `issue`, if any.
    pub fn active_claim(&self, issue: &IssueRef, now: DateTime<Utc>) -> Result<Option<Claim>> {
        self.conn
            .query_row(
                &format!(
                    "SELECT {CLAIM_COLUMNS} FROM claims
                     WHERE issue_ref = ?1 AND status = 'active' AND lease_until > ?2"
                ),
                params![issue.to_string(), now.timestamp()],
                claim_from_row,
            )
            .optional()?
            .map(ClaimRow::into_claim)
            .transpose()
    }

    /// Active claims, optionally for one agent.
    pub fn claims(&self, agent: Option<&AgentId>, now: DateTime<Utc>) -> Result<Vec<Claim>> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT {CLAIM_COLUMNS} FROM claims
             WHERE status = 'active' AND lease_until > ?1 AND (?2 IS NULL OR agent_id = ?2)
             ORDER BY lease_until, issue_ref"
        ))?;
        let rows = statement
            .query_map(
                params![now.timestamp(), agent.map(AgentId::as_str)],
                claim_from_row,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter().map(ClaimRow::into_claim).collect()
    }

    /// Marks leases past `lease_until` as expired; returns how many.
    pub fn expire_claims(&mut self, now: DateTime<Utc>) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE claims SET status = 'expired', updated_at = ?1
             WHERE status = 'active' AND lease_until <= ?1",
            params![now.timestamp()],
        )?)
    }

    /// Appends a gate result with its evidence snapshot (the whole report as JSON).
    pub fn log_gate(&mut self, report: &GateReport) -> Result<()> {
        let verdict = serde_json::to_value(report.verdict)?;
        self.conn.execute(
            "INSERT INTO gate_log (kind, issue_ref, agent_id, verdict, reasons_json, snapshot_json, as_of)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                report.kind.as_str(),
                report.issue.to_string(),
                report.agent.as_str(),
                verdict.as_str().unwrap_or_default(),
                serde_json::to_string(&report.blocked_by)?,
                serde_json::to_string(report)?,
                report.as_of.timestamp(),
            ],
        )?;
        Ok(())
    }
}
