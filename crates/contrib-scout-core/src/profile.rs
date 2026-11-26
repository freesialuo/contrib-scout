//! Repository profiles: metadata and contributability metrics (§1, §6, §7). Step 4 owns this file.
//!
//! Cost model:
//! - [`RepoProfiler::profile`]: one conditional `GET /repos/{owner}/{repo}`,
//!   stored in `repos` (and the gateway's `http_cache`) for `repo_ttl`.
//! - [`RepoProfiler::activity`]: ONE GraphQL query ([`ACTIVITY_QUERY`]) over the
//!   last [`ACTIVITY_PRS`] closed/merged PRs and the last [`ACTIVITY_ISSUES`]
//!   issues, stored in `repo_activity` for `activity_ttl`. Callers compute it
//!   only for repositories that are not marked `Bans` (§7).
//!
//! Metric definitions (all from that one query):
//! - external PR: `authorAssociation` is `FIRST_TIMER`, `FIRST_TIME_CONTRIBUTOR`,
//!   `CONTRIBUTOR`, or `NONE`; maintainer: `OWNER`, `MEMBER`, or `COLLABORATOR`.
//! - `external_pr_merge_rate` = merged / closed-or-merged external PRs
//!   (`None` when the sample has none); `sample_size` = that denominator.
//! - `median_first_response_secs`: median over external PRs and non-maintainer
//!   issues of the delay until the first maintainer comment or review (for PRs,
//!   a merge also counts as a response); items with no response are skipped.
//! - `maintainer_last_active`: latest maintainer comment, review, merge, or
//!   maintainer-authored PR/issue in the sample.

use crate::config::Config;
use crate::error::{Result, ScoutError};
use crate::freshness::{Freshness, Live, LiveRead, Origin};
use crate::gateway::Gateway;
use crate::models::{RepoActivity, RepoName, RepoProfile};
use crate::store::SharedStore;
use crate::transport::ApiRequest;
use anyhow::{anyhow, Context};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

pub trait RepoProfiler {
    /// Metadata (TTL 7d, conditional request) plus cached activity if present.
    fn profile(&self, repo: &RepoName, mode: Freshness) -> Result<RepoProfile>;

    /// External PR merge rate, median first response, maintainer activity.
    /// Uses search and GraphQL — the most expensive data; compute only for
    /// repositories that passed the policy check (§7).
    fn activity(&self, repo: &RepoName, mode: Freshness) -> Result<RepoActivity>;

    /// Gate variant: live metadata (archived check, §8).
    fn profile_live(&self, repo: &RepoName, live: LiveRead) -> Result<Live<RepoProfile>>;
}

/// Closed/merged PRs sampled by the activity query.
pub const ACTIVITY_PRS: u32 = 50;
/// Recent issues sampled by the activity query.
pub const ACTIVITY_ISSUES: u32 = 30;

/// The single GraphQL query behind [`RepoProfiler::activity`].
pub const ACTIVITY_QUERY: &str = "query RepoActivity($owner: String!, $name: String!, $prs: Int!, $issues: Int!) {
  repository(owner: $owner, name: $name) {
    pullRequests(first: $prs, states: [MERGED, CLOSED], orderBy: {field: UPDATED_AT, direction: DESC}) {
      nodes {
        authorAssociation
        merged
        createdAt
        mergedAt
        author { login }
        comments(first: 10) { nodes { authorAssociation createdAt author { login } } }
        reviews(first: 10) { nodes { authorAssociation createdAt author { login } } }
      }
    }
    issues(first: $issues, orderBy: {field: CREATED_AT, direction: DESC}) {
      nodes {
        authorAssociation
        createdAt
        author { login }
        comments(first: 10) { nodes { authorAssociation createdAt author { login } } }
      }
    }
  }
}";

pub struct GitHubProfiler {
    pub gateway: Arc<Gateway>,
    pub store: SharedStore,
    pub config: Config,
    /// Stale-cache notes from the gateway, drained by [`GitHubProfiler::take_warnings`].
    warnings: Mutex<Vec<String>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn age(at: DateTime<Utc>) -> Duration {
    (Utc::now() - at).to_std().unwrap_or_default()
}

impl GitHubProfiler {
    pub fn new(gateway: Arc<Gateway>, store: SharedStore, config: Config) -> Self {
        Self {
            gateway,
            store,
            config,
            warnings: Mutex::new(Vec::new()),
        }
    }

    /// Warnings (served-stale notes) collected since the last call. Callers
    /// add them to `meta.warnings` and set `meta.partial`.
    pub fn take_warnings(&self) -> Vec<String> {
        std::mem::take(&mut *lock(&self.warnings))
    }

    fn request(repo: &RepoName) -> ApiRequest {
        ApiRequest::get(format!("/repos/{}/{}", repo.owner(), repo.name()))
    }

    /// Whether `profile` would be answered locally (no request) in `mode`.
    pub fn profile_is_cached(&self, repo: &RepoName, mode: Freshness) -> Result<bool> {
        let cached = lock(&self.store).repo(repo)?;
        Ok(match (mode, cached) {
            (Freshness::Offline, _) => true,
            (_, Some((cached, _))) => {
                !mode.needs_fetch(Some(age(cached.fetched_at)), self.config.ttls().repo)
            }
            (_, None) => false,
        })
    }

    /// Whether `activity` would be answered locally (no request) in `mode`.
    pub fn activity_is_cached(&self, repo: &RepoName, mode: Freshness) -> Result<bool> {
        let cached = lock(&self.store).activity(repo)?;
        Ok(match (mode, cached) {
            (Freshness::Offline, _) => true,
            (_, Some(cached)) => {
                !mode.needs_fetch(Some(age(cached.computed_at)), self.config.ttls().activity)
            }
            (_, None) => false,
        })
    }

    fn with_activity(&self, mut profile: RepoProfile) -> Result<RepoProfile> {
        profile.activity = lock(&self.store).activity(&profile.repo)?;
        Ok(profile)
    }
}

impl RepoProfiler for GitHubProfiler {
    fn profile(&self, repo: &RepoName, mode: Freshness) -> Result<RepoProfile> {
        let ttl = self.config.ttls().repo;
        let cached = lock(&self.store).repo(repo)?;
        if let Some((cached, _)) = &cached {
            let fresh = !mode.needs_fetch(Some(age(cached.fetched_at)), ttl);
            if fresh || mode == Freshness::Offline {
                return self.with_activity(cached.data.clone());
            }
        }
        let fetched = self.gateway.get(&Self::request(repo), mode, ttl)?;
        if let Some(note) = &fetched.stale {
            lock(&self.warnings).push(note.clone());
        }
        if fetched.is_not_found() {
            return Err(ScoutError::NotFound {
                what: format!("repository {repo}"),
            }
            .into());
        }
        let raw: Value = fetched
            .response
            .json_body()
            .with_context(|| format!("parse repository {repo}"))?;
        let profile = parse_profile(repo, &raw, fetched.fetched_at)?;
        {
            let mut store = lock(&self.store);
            match (fetched.origin, &cached) {
                (Origin::NotModified, Some(_)) => store.touch_repo(repo, fetched.fetched_at)?,
                _ => store.save_repo(&profile, &raw, fetched.response.etag())?,
            }
        }
        self.with_activity(profile)
    }

    fn activity(&self, repo: &RepoName, mode: Freshness) -> Result<RepoActivity> {
        let ttl = self.config.ttls().activity;
        let cached = lock(&self.store).activity(repo)?;
        match (&cached, mode) {
            (Some(cached), Freshness::Offline) => return Ok(cached.clone()),
            (None, Freshness::Offline) => {
                return Err(ScoutError::NotFound {
                    what: format!("cached activity for {repo} (offline)"),
                }
                .into())
            }
            (Some(cached), _) if !mode.needs_fetch(Some(age(cached.computed_at)), ttl) => {
                return Ok(cached.clone())
            }
            _ => {}
        }
        let variables = json!({
            "owner": repo.owner(),
            "name": repo.name(),
            "prs": ACTIVITY_PRS,
            "issues": ACTIVITY_ISSUES,
        });
        let response = self.gateway.graphql(ACTIVITY_QUERY, &variables)?;
        let body: Value = response
            .json_body()
            .with_context(|| format!("parse activity GraphQL response for {repo}"))?;
        check_graphql_errors(repo, &body)?;
        let data = body
            .pointer("/data/repository")
            .filter(|v| !v.is_null())
            .ok_or_else(|| ScoutError::NotFound {
                what: format!("repository {repo} (GraphQL)"),
            })?;
        let activity = compute_activity(data, Utc::now());
        lock(&self.store).save_activity(repo, &activity, Some(data))?;
        Ok(activity)
    }

    fn profile_live(&self, repo: &RepoName, live: LiveRead) -> Result<Live<RepoProfile>> {
        let response = self.gateway.get_live(&Self::request(repo), live)?;
        if response.get().status == 404 {
            return Err(ScoutError::NotFound {
                what: format!("repository {repo}"),
            }
            .into());
        }
        let raw: Value = response
            .get()
            .json_body()
            .with_context(|| format!("parse repository {repo}"))?;
        let profile = parse_profile(repo, &raw, response.as_of())?;
        lock(&self.store).save_repo(&profile, &raw, response.get().etag())?;
        let profile = self.with_activity(profile)?;
        Ok(response.map(|_| profile))
    }
}

/// Fails with every GraphQL `errors[].message` when the array is non-empty.
pub fn check_graphql_errors(repo: &RepoName, body: &Value) -> Result<()> {
    let Some(errors) = body.get("errors").and_then(Value::as_array) else {
        return Ok(());
    };
    if errors.is_empty() {
        return Ok(());
    }
    let not_found = errors
        .iter()
        .all(|e| e.get("type").and_then(Value::as_str) == Some("NOT_FOUND"));
    if not_found && body.pointer("/data/repository").is_none_or(Value::is_null) {
        return Err(ScoutError::NotFound {
            what: format!("repository {repo} (GraphQL)"),
        }
        .into());
    }
    let messages: Vec<&str> = errors
        .iter()
        .map(|e| {
            e.get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error")
        })
        .collect();
    Err(anyhow!(
        "GraphQL errors in activity query for {repo}: {}",
        messages.join("; ")
    ))
}

fn time(value: Option<&Value>) -> Option<DateTime<Utc>> {
    value
        .and_then(Value::as_str)
        .and_then(|text| DateTime::parse_from_rfc3339(text).ok())
        .map(|t| t.with_timezone(&Utc))
}

/// Builds a profile from `GET /repos/{owner}/{repo}` JSON.
pub fn parse_profile(
    repo: &RepoName,
    raw: &Value,
    fetched_at: DateTime<Utc>,
) -> Result<RepoProfile> {
    if !raw.is_object() {
        return Err(anyhow!("repository {repo}: expected a JSON object"));
    }
    let text = |key: &str| raw.get(key).and_then(Value::as_str).map(str::to_owned);
    let license = raw
        .get("license")
        .filter(|l| !l.is_null())
        .and_then(|license| {
            license
                .get("spdx_id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty() && *id != "NOASSERTION")
                .or_else(|| license.get("name").and_then(Value::as_str))
                .map(str::to_owned)
        });
    Ok(RepoProfile {
        repo: repo.clone(),
        description: text("description"),
        archived: raw
            .get("archived")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        fork: raw.get("fork").and_then(Value::as_bool).unwrap_or(false),
        license,
        default_branch: text("default_branch"),
        language: text("language"),
        topics: raw
            .get("topics")
            .and_then(Value::as_array)
            .map(|topics| {
                topics
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
        stars: raw
            .get("stargazers_count")
            .and_then(Value::as_u64)
            .unwrap_or_default(),
        pushed_at: time(raw.get("pushed_at")),
        fetched_at,
        activity: None,
    })
}

const EXTERNAL: &[&str] = &[
    "FIRST_TIMER",
    "FIRST_TIME_CONTRIBUTOR",
    "CONTRIBUTOR",
    "NONE",
];
const MAINTAINER: &[&str] = &["OWNER", "MEMBER", "COLLABORATOR"];

fn association(node: &Value) -> &str {
    node.get("authorAssociation")
        .and_then(Value::as_str)
        .unwrap_or("NONE")
}

fn login(node: &Value) -> Option<&str> {
    node.pointer("/author/login").and_then(Value::as_str)
}

fn nodes<'a>(value: &'a Value, path: &str) -> impl Iterator<Item = &'a Value> {
    value
        .pointer(path)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

/// Maintainer responses (comments, then reviews) to `item` not written by its author.
fn maintainer_responses(item: &Value, paths: &[&str]) -> Vec<DateTime<Utc>> {
    let author = login(item);
    let mut times = Vec::new();
    for path in paths {
        for node in nodes(item, path) {
            if MAINTAINER.contains(&association(node))
                && (author.is_none() || login(node) != author)
            {
                if let Some(at) = time(node.get("createdAt")) {
                    times.push(at);
                }
            }
        }
    }
    times
}

/// Computes [`RepoActivity`] from the activity query's `data.repository`.
pub fn compute_activity(repository: &Value, now: DateTime<Utc>) -> RepoActivity {
    let mut external = 0u32;
    let mut merged = 0u32;
    let mut delays: Vec<i64> = Vec::new();
    let mut last_active: Option<DateTime<Utc>> = None;
    let mut seen = |at: DateTime<Utc>| {
        last_active = Some(last_active.map_or(at, |last| last.max(at)));
    };

    for pr in nodes(repository, "/pullRequests/nodes") {
        let created = time(pr.get("createdAt"));
        let merged_at = time(pr.get("mergedAt"));
        let responses = maintainer_responses(pr, &["/comments/nodes", "/reviews/nodes"]);
        for at in responses.iter().copied().chain(merged_at) {
            seen(at);
        }
        let assoc = association(pr);
        if MAINTAINER.contains(&assoc) {
            if let Some(created) = created {
                seen(created);
            }
        }
        if !EXTERNAL.contains(&assoc) {
            continue;
        }
        external += 1;
        if pr.get("merged").and_then(Value::as_bool).unwrap_or(false) {
            merged += 1;
        }
        let first = responses.into_iter().chain(merged_at).min();
        if let (Some(created), Some(first)) = (created, first) {
            delays.push((first - created).num_seconds().max(0));
        }
    }

    for issue in nodes(repository, "/issues/nodes") {
        let created = time(issue.get("createdAt"));
        let responses = maintainer_responses(issue, &["/comments/nodes"]);
        for at in &responses {
            seen(*at);
        }
        if MAINTAINER.contains(&association(issue)) {
            if let Some(created) = created {
                seen(created);
            }
            continue;
        }
        if let (Some(created), Some(first)) = (created, responses.into_iter().min()) {
            delays.push((first - created).num_seconds().max(0));
        }
    }

    RepoActivity {
        external_pr_merge_rate: (external > 0).then(|| f64::from(merged) / f64::from(external)),
        median_first_response_secs: median(&mut delays)
            .map(|s| u64::try_from(s).unwrap_or_default()),
        maintainer_last_active: last_active,
        sample_size: external,
        computed_at: now,
    }
}

fn median(values: &mut [i64]) -> Option<i64> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    let mid = values.len() / 2;
    Some(if values.len().is_multiple_of(2) {
        (values[mid - 1] + values[mid]) / 2
    } else {
        values[mid]
    })
}
