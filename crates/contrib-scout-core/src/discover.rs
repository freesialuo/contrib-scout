//! Discovery pipeline and local candidates (§7). Step 4 owns this file.
//!
//! Cheapest first, stop at the first elimination:
//! search (cached 24h) → local policy marks (zero requests; `Bans` excluded and
//! counted unless `include_marked`) → repo metadata (drop archived and forks) →
//! activity → issue index → local scoring. Unmarked repos are listed with an
//! `unmarked` reminder. Discovery never fetches policy text.

use crate::error::{not_implemented, Result};
use crate::freshness::Freshness;
use crate::models::{IssueRow, RepoName, RepoProfile};
use serde::{Deserialize, Serialize};

/// `discover` arguments.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoverQuery {
    pub language: Option<String>,
    pub topic: Option<String>,
    pub labels: Vec<String>,
    pub limit: u32,
    /// Include per-stage elimination reasons.
    pub explain: bool,
    /// Do not exclude `Bans` repos.
    pub include_marked: bool,
}

/// Pipeline stage at which a repository was dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Search,
    PolicyMark,
    Metadata,
    Activity,
    Index,
    Score,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Exclusion {
    pub repo: RepoName,
    pub stage: Stage,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    pub issue: IssueRow,
    pub score: f64,
    #[serde(default)]
    pub reasons: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DiscoveredRepo {
    pub profile: RepoProfile,
    pub score: f64,
    pub candidates: Vec<Candidate>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DiscoverReport {
    pub repos: Vec<DiscoveredRepo>,
    /// Repos excluded because of a `Bans` mark ("excluded N", §5 step 2).
    pub excluded_marked: u32,
    /// Populated only with `explain`.
    #[serde(default)]
    pub excluded: Vec<Exclusion>,
    /// Repos not seen in the previous run for this query (exit code 10).
    pub new_repos: u32,
}

/// `candidates` arguments; local data only, zero requests.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CandidateQuery {
    pub min_score: Option<f64>,
    pub language: Option<String>,
    pub limit: u32,
    pub include_marked: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CandidatesReport {
    pub candidates: Vec<Candidate>,
    pub excluded_marked: u32,
}

pub trait Discovery {
    fn discover(&self, query: &DiscoverQuery, mode: Freshness) -> Result<DiscoverReport>;

    /// Reads only the local store; must make zero requests (§13).
    fn candidates(&self, query: &CandidateQuery) -> Result<CandidatesReport>;
}

/// Default pipeline over the profiler, indexer, marks, and store.
pub struct Pipeline {
    pub gateway: std::sync::Arc<crate::gateway::Gateway>,
    pub store: crate::store::SharedStore,
    pub config: crate::config::Config,
}

impl Discovery for Pipeline {
    fn discover(&self, _query: &DiscoverQuery, _mode: Freshness) -> Result<DiscoverReport> {
        Err(not_implemented("discover"))
    }

    fn candidates(&self, _query: &CandidateQuery) -> Result<CandidatesReport> {
        Err(not_implemented("candidates"))
    }
}
