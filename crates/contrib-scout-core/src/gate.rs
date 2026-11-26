//! Live gates: always live, fail closed, raw data out (§8). Step 6 owns this file.
//!
//! Type-level rule (§4): a [`Gatekeeper`] takes no [`crate::Freshness`]. Its
//! inputs are fetched through the `*_live` methods, which take a
//! [`crate::LiveRead`] and return [`crate::Live<T>`]; [`GateInputs`] only holds
//! `Live<T>`, so cached data cannot reach [`evaluate`].
//!
//! Network or quota failure ⇒ `Err`, which the CLI renders as verdict `error`
//! (exit 1). Unmarked repo ⇒ `blocked: policy_unmarked`. `Bans` ⇒ `policy_bans`.
//! `gate pr` with `pr_text: human_written` or `autonomous_agents: forbidden`
//! ⇒ `human_required`.

use crate::error::{not_implemented, Result};
use crate::freshness::Live;
use crate::models::{
    AgentId, Claim, GateFinding, GateKind, GateReport, GateVerdict, IssueDetail, IssueRef,
    PolicyFile, PolicyMark, RepoProfile,
};

pub trait Gatekeeper {
    /// Live checks before starting work; acquires or renews the local claim.
    fn start(&self, issue: &IssueRef, agent: &AgentId) -> Result<GateReport>;

    /// Live checks before opening a PR; the claim must be held. `branch` is the
    /// agent's own head branch, excluded from "linked PR exists".
    fn pr(&self, issue: &IssueRef, agent: &AgentId, branch: Option<&str>) -> Result<GateReport>;
}

/// Everything a gate decides from. Only live data, plus local mark and claim state.
pub struct GateInputs {
    pub issue: Live<IssueDetail>,
    pub repo: Live<RepoProfile>,
    pub policy_files: Live<Vec<PolicyFile>>,
    /// Local, agent-written (§5); not GitHub data.
    pub mark: Option<PolicyMark>,
    /// The claim after acquire/renew (`start`) or lookup (`pr`).
    pub claim: Option<Claim>,
    /// Another agent's active claim, if any.
    pub claimed_by_other: Option<Claim>,
}

/// Pure decision over live inputs; no I/O.
pub fn evaluate(
    _kind: GateKind,
    _issue: &IssueRef,
    _agent: &AgentId,
    _branch: Option<&str>,
    _inputs: GateInputs,
) -> Result<GateReport> {
    Err(not_implemented("gate evaluation"))
}

/// `pass` with no findings, else `blocked`.
pub fn verdict_for(findings: &[GateFinding]) -> GateVerdict {
    if findings.is_empty() {
        GateVerdict::Pass
    } else {
        GateVerdict::Blocked
    }
}

/// The default gatekeeper over the live service methods.
pub struct LiveGatekeeper {
    pub policy_files: std::sync::Arc<dyn crate::policy::PolicyFiles + Send + Sync>,
    pub marks: std::sync::Arc<dyn crate::policy::PolicyMarks + Send + Sync>,
    pub profiler: std::sync::Arc<dyn crate::profile::RepoProfiler + Send + Sync>,
    pub indexer: std::sync::Arc<dyn crate::index::IssueIndexer + Send + Sync>,
    pub claims: std::sync::Arc<crate::claims::ClaimService>,
}

impl Gatekeeper for LiveGatekeeper {
    fn start(&self, _issue: &IssueRef, _agent: &AgentId) -> Result<GateReport> {
        Err(not_implemented("gate start"))
    }

    fn pr(&self, _issue: &IssueRef, _agent: &AgentId, _branch: Option<&str>) -> Result<GateReport> {
        Err(not_implemented("gate pr"))
    }
}
