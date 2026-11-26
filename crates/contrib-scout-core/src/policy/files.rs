//! Candidate policy file fetching (§2, §5 step 4). Step 3 owns this file.

use super::PolicyFiles;
use crate::config::Config;
use crate::error::{not_implemented, Result};
use crate::freshness::{Freshness, Live, LiveRead};
use crate::gateway::Gateway;
use crate::models::{PolicyFile, RepoName};
use crate::store::SharedStore;
use std::sync::Arc;

/// Where AI policies are found (§2). Checked in both `{owner}/.github` and the
/// repository. GitHub paths are case-sensitive; an implementation may list the
/// root, `.github/`, and `docs/` and match these case-insensitively instead.
pub const CANDIDATE_PATHS: &[&str] = &[
    "AI_POLICY.md",
    "LLM_POLICY.md",
    ".github/AI_POLICY.md",
    "CONTRIBUTING.md",
    ".github/CONTRIBUTING.md",
    "docs/CONTRIBUTING.md",
    "AGENTS.md",
    "CLAUDE.md",
    ".github/copilot-instructions.md",
    ".github/PULL_REQUEST_TEMPLATE.md",
    ".github/pull_request_template.md",
    "PULL_REQUEST_TEMPLATE.md",
    "docs/pull_request_template.md",
    "README.md",
];

/// `(repository, path)` pairs to probe: org-level first, then repo-level, so
/// a repository policy can override the org's (§2). Skips the org pass when
/// `repo` is itself `{owner}/.github`.
pub fn candidate_locations(repo: &RepoName) -> Vec<(RepoName, &'static str)> {
    let org = repo.org_policy_repo();
    let mut sources = Vec::new();
    if !org.name().eq_ignore_ascii_case(repo.name()) {
        sources.push(org);
    }
    sources.push(repo.clone());
    sources
        .into_iter()
        .flat_map(|source| {
            CANDIDATE_PATHS
                .iter()
                .map(move |path| (source.clone(), *path))
        })
        .collect()
}

pub struct GitHubPolicyFiles {
    pub gateway: Arc<Gateway>,
    pub store: SharedStore,
    pub config: Config,
}

impl PolicyFiles for GitHubPolicyFiles {
    fn fetch(&self, _repo: &RepoName, _mode: Freshness) -> Result<Vec<PolicyFile>> {
        Err(not_implemented("policy files"))
    }

    fn fetch_live(
        &self,
        _repo: &RepoName,
        _live: LiveRead,
        _with_text: bool,
    ) -> Result<Live<Vec<PolicyFile>>> {
        Err(not_implemented("policy files"))
    }
}
