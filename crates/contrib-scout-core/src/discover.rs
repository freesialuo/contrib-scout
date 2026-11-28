//! Discovery pipeline and local candidates (§7). Step 4 owns this file.
//!
//! Cheapest first, stop at the first elimination:
//! search (cached `search_ttl` in `discover_runs`, one page) → local policy
//! marks (zero requests; `Bans` excluded and counted unless `include_marked`)
//! → repo metadata (drop archived, forks, topic mismatches, and repos not
//! pushed for [`STALE_DAYS`]) → activity (skipped for `Bans` repos) → issue
//! index ([`IssueIndexer::sync`], then local rows) → local scoring. Unmarked
//! repos are listed with an `unmarked` reminder. Discovery never fetches
//! policy text.
//!
//! # Scoring (0–100, see [`repo_score`] and [`issue_score`])
//! Repository part (0–50), from the profile's activity:
//! - external PR merge rate × 20 (unknown: 8);
//! - median maintainer first response: ≤1d 15, ≤7d 10, ≤30d 5, slower 0 (unknown: 5);
//! - last maintainer activity: ≤30d 15, ≤90d 8, ≤365d 3, older 0
//!   (unknown: same scale on `pushed_at`, halved).
//!
//! Issue part (0–50):
//! - labels: a beginner label (`good first issue`, `beginner`, `easy`, …) 15,
//!   `help wanted` 10 (both: 25);
//! - freshness of `updated_at`: ≤7d 15, ≤30d 10, ≤90d 5, older 0;
//! - comments (less contention is better): 0 → 10, ≤3 → 7, ≤10 → 3, more 0.
//!
//! `shortlist`-marked issues rank first regardless of score and are exempt
//! from `min_score`; active `skip`-marked issues are excluded unless
//! `include_marked` (§5b).
//!
//! `candidates` also hides issues with an active claim (§9) held by another
//! agent — every claimed issue when the caller has no agent id — unless
//! `include_claimed`, and counts them in `excluded_claimed`; listed issues
//! with an active claim carry it in `claim`.

use crate::config::Config;
use crate::error::{Result, ScoutError};
use crate::freshness::Freshness;
use crate::gateway::Gateway;
use crate::index::{GitHubIndexer, IssueIndexer};
use crate::models::{
    normalize_label, AgentId, Claim, IssueRow, PolicyMark, Reminder, RepoActivity, RepoName,
    RepoProfile, Stance,
};
use crate::policy::reminders::reminder_for;
use crate::profile::{GitHubProfiler, RepoProfiler};
use crate::store::repos::StoredIssueMark;
use crate::store::{DiscoverRun, SharedStore, Store};
use crate::transport::ApiRequest;
use anyhow::Context;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, MutexGuard};

/// Label searched for when `discover` gets no `--label`.
pub const DEFAULT_LABEL: &str = "good first issue";
/// Results per search page; discovery reads one page per run.
pub const SEARCH_PER_PAGE: u32 = 100;
/// Repositories not pushed (metadata) or without maintainer activity for this long are dropped.
pub const STALE_DAYS: i64 = 365;
/// External PRs needed before a 0% merge rate eliminates a repository.
pub const MIN_MERGE_SAMPLE: u32 = 5;
/// Candidate issues listed per discovered repository.
pub const CANDIDATES_PER_REPO: usize = 10;

/// `discover` arguments.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoverQuery {
    pub language: Option<String>,
    /// Matched against the repository's topics at the metadata stage (the
    /// issue search API has no topic qualifier), so it costs no extra request.
    pub topic: Option<String>,
    /// Issue labels, OR-ed; empty means [`DEFAULT_LABEL`].
    pub labels: Vec<String>,
    /// Maximum repositories kept.
    pub limit: u32,
    /// Print the plan and estimated request count instead of running ([`Discovery::plan`]).
    pub explain: bool,
    /// Do not exclude `Bans` repos or `skip`-marked issues.
    pub include_marked: bool,
}

impl DiscoverQuery {
    pub fn effective_labels(&self) -> Vec<String> {
        if self.labels.is_empty() {
            vec![DEFAULT_LABEL.to_owned()]
        } else {
            self.labels.clone()
        }
    }
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

/// The issue mark that applied to a candidate (§5b).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidateMark {
    pub verdict: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub note: String,
    pub marked_by: String,
    pub marked_at: DateTime<Utc>,
    /// `updated_at` or the comment count changed since the mark's snapshot.
    pub changed_since_mark: bool,
}

impl CandidateMark {
    fn from_stored(mark: &StoredIssueMark, row: &IssueRow) -> Self {
        Self {
            verdict: mark.verdict.clone(),
            reason: mark.reason.clone(),
            note: mark.note.clone(),
            marked_by: mark.marked_by.clone(),
            marked_at: mark.marked_at,
            changed_since_mark: mark.changed_since(row),
        }
    }

    pub fn is_shortlist(&self) -> bool {
        self.verdict.eq_ignore_ascii_case("shortlist")
    }
}

/// The active (unexpired, not released) local claim on a candidate (§9).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidateClaim {
    pub agent: AgentId,
    pub lease_until: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    pub issue: IssueRow,
    pub score: f64,
    #[serde(default)]
    pub reasons: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mark: Option<CandidateMark>,
    /// Set by `candidates` when the issue has an active claim; `discover` leaves it empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim: Option<CandidateClaim>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DiscoveredRepo {
    pub profile: RepoProfile,
    /// The best candidate's score.
    pub score: f64,
    pub candidates: Vec<Candidate>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DiscoverReport {
    /// The issue search query.
    pub query: String,
    pub repos: Vec<DiscoveredRepo>,
    /// Repos excluded because of a `Bans` mark ("excluded N", §5 step 2).
    pub excluded_marked: u32,
    /// Issues excluded because of an active `skip` mark (§5b).
    #[serde(default)]
    pub excluded_skipped: u32,
    /// Every repository dropped, with the stage and reason.
    #[serde(default)]
    pub excluded: Vec<Exclusion>,
    /// Repos not seen in the previous run for this query (exit code 10).
    pub new_repos: u32,
    /// One reminder per repository touched (§5).
    #[serde(default)]
    pub reminders: Vec<Reminder>,
    /// Stale-cache notes and per-repository failures; `partial` is set with them.
    #[serde(default)]
    pub warnings: Vec<String>,
    #[serde(default)]
    pub partial: bool,
}

/// One step of a `discover --explain` plan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanStage {
    pub stage: Stage,
    pub description: String,
    pub estimated_requests: u32,
}

/// What `discover` would do, computed from local data only (zero requests).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoverPlan {
    pub query: String,
    pub search_path: String,
    /// A usable cached search run exists, so its repositories are known.
    pub search_cached: bool,
    /// Repositories from the cached run (empty when not cached).
    pub known_repos: Vec<RepoName>,
    pub stages: Vec<PlanStage>,
    /// Upper bound: assumes no repository is eliminated before the index stage.
    pub estimated_requests: u32,
    #[serde(default)]
    pub reminders: Vec<Reminder>,
}

/// `candidates` arguments; local data only, zero requests.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CandidateQuery {
    pub min_score: Option<f64>,
    pub language: Option<String>,
    pub limit: u32,
    pub include_marked: bool,
    /// The calling agent. Issues with an active claim held by anyone else are
    /// hidden unless `include_claimed`; without an id every claimed issue is.
    #[serde(default)]
    pub agent: Option<AgentId>,
    /// Do not hide issues claimed by other agents.
    #[serde(default)]
    pub include_claimed: bool,
    /// Only these repositories (case-insensitive); empty means all.
    #[serde(default)]
    pub repos: Vec<RepoName>,
    /// The issue must carry every one of these labels (case-insensitive).
    #[serde(default)]
    pub labels: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CandidatesReport {
    pub candidates: Vec<Candidate>,
    /// Issues excluded because their repository (or org) is marked `Bans`.
    pub excluded_marked: u32,
    /// Issues excluded because of an active `skip` mark.
    #[serde(default)]
    pub excluded_skipped: u32,
    /// Issues that passed every other filter but are hidden because another
    /// agent (or, without an agent id, anyone) holds an active claim.
    #[serde(default)]
    pub excluded_claimed: u32,
    /// One reminder per repository in the output or excluded by a mark.
    #[serde(default)]
    pub reminders: Vec<Reminder>,
}

pub trait Discovery {
    fn discover(&self, query: &DiscoverQuery, mode: Freshness) -> Result<DiscoverReport>;

    /// The plan and estimated request count for `discover`, without fetching.
    fn plan(&self, query: &DiscoverQuery, mode: Freshness) -> Result<DiscoverPlan>;

    /// Reads only the local store; must make zero requests (§13).
    fn candidates(&self, query: &CandidateQuery) -> Result<CandidatesReport>;
}

/// Default pipeline over the profiler, indexer, marks, and store.
pub struct Pipeline {
    pub gateway: Arc<Gateway>,
    pub store: SharedStore,
    pub config: Config,
    pub profiler: GitHubProfiler,
    pub indexer: Arc<dyn IssueIndexer>,
}

fn lock(store: &SharedStore) -> MutexGuard<'_, Store> {
    store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The issue search query for `query` (labels OR-ed, language qualifier).
pub fn search_query(query: &DiscoverQuery) -> String {
    let labels: Vec<String> = query
        .effective_labels()
        .iter()
        .map(|label| format!("\"{}\"", label.replace('"', "")))
        .collect();
    let mut q = format!(
        "is:issue is:open no:assignee archived:false label:{}",
        labels.join(",")
    );
    if let Some(language) = query.language.as_deref().filter(|l| !l.trim().is_empty()) {
        let language = language.trim();
        if language.contains(' ') {
            q.push_str(&format!(" language:\"{}\"", language.replace('"', "")));
        } else {
            q.push_str(&format!(" language:{language}"));
        }
    }
    q
}

fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 3);
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(char::from(byte));
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// `GET` path of the single search page for `q`.
pub fn search_path(q: &str) -> String {
    format!(
        "/search/issues?q={}&sort=updated&order=desc&per_page={SEARCH_PER_PAGE}",
        percent_encode(q)
    )
}

fn run_key(q: &str) -> String {
    format!("issues:{q}")
}

/// Repositories of the search items, in first-seen order (PRs skipped).
fn repos_from_search(body: &Value) -> Vec<RepoName> {
    let mut seen = HashSet::new();
    let mut repos = Vec::new();
    for item in body
        .get("items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if item.get("pull_request").is_some_and(|v| !v.is_null()) {
            continue;
        }
        let Some(url) = item.get("repository_url").and_then(Value::as_str) else {
            continue;
        };
        let Some(name) = url.split("/repos/").nth(1) else {
            continue;
        };
        if let Ok(repo) = name.parse::<RepoName>() {
            if seen.insert(repo.key()) {
                repos.push(repo);
            }
        }
    }
    repos
}

fn age(at: DateTime<Utc>, now: DateTime<Utc>) -> std::time::Duration {
    (now - at).to_std().unwrap_or_default()
}

fn is_bans(mark: Option<&PolicyMark>) -> bool {
    mark.is_some_and(|mark| mark.stance == Stance::Bans)
}

/// Budget and quota errors stop the whole run; other errors only drop one repo.
fn stops_run(error: &anyhow::Error) -> bool {
    matches!(
        ScoutError::find(error),
        Some(
            ScoutError::BudgetExhausted { .. }
                | ScoutError::QuotaReserved { .. }
                | ScoutError::WindowLimit { .. }
        )
    )
}

fn is_not_found(error: &anyhow::Error) -> bool {
    matches!(ScoutError::find(error), Some(ScoutError::NotFound { .. }))
}

const BEGINNER_LABELS: &[&str] = &[
    "good first issue",
    "good first bug",
    "first timers only",
    "beginner",
    "beginner friendly",
    "easy",
    "starter",
];

fn recency_points(days: i64, scale: &[(i64, f64)]) -> f64 {
    scale
        .iter()
        .find(|(limit, _)| days <= *limit)
        .map_or(0.0, |(_, points)| *points)
}

/// Repository part of the score (0–50) with human-readable reasons.
pub fn repo_score(profile: &RepoProfile, now: DateTime<Utc>) -> (f64, Vec<String>) {
    let mut reasons = Vec::new();
    let activity = profile.activity.as_ref();
    let merge = match activity.and_then(|a| a.external_pr_merge_rate) {
        Some(rate) => {
            reasons.push(format!(
                "external PR merge rate {:.0}% (n={})",
                rate * 100.0,
                activity.map_or(0, |a| a.sample_size)
            ));
            rate.clamp(0.0, 1.0) * 20.0
        }
        None => 8.0,
    };
    let response = match activity.and_then(|a| a.median_first_response_secs) {
        Some(secs) => {
            let days = secs / 86_400;
            reasons.push(format!(
                "median maintainer first response {}",
                human_secs(secs)
            ));
            if secs <= 86_400 {
                15.0
            } else if days <= 7 {
                10.0
            } else if days <= 30 {
                5.0
            } else {
                0.0
            }
        }
        None => 5.0,
    };
    const RECENCY: &[(i64, f64)] = &[(30, 15.0), (90, 8.0), (365, 3.0)];
    let recency = match activity.and_then(|a| a.maintainer_last_active) {
        Some(at) => {
            let days = (now - at).num_days();
            reasons.push(format!("maintainer active {days}d ago"));
            recency_points(days, RECENCY)
        }
        None => profile.pushed_at.map_or(0.0, |at| {
            recency_points((now - at).num_days(), RECENCY) / 2.0
        }),
    };
    (merge + response + recency, reasons)
}

/// Issue part of the score (0–50) with human-readable reasons.
pub fn issue_score(issue: &IssueRow, now: DateTime<Utc>) -> (f64, Vec<String>) {
    let mut reasons = Vec::new();
    let labels: Vec<String> = issue.labels.iter().map(|l| normalize_label(l)).collect();
    let mut label_points = 0.0;
    if labels.iter().any(|l| BEGINNER_LABELS.contains(&l.as_str())) {
        label_points += 15.0;
        reasons.push("beginner label".to_owned());
    }
    if labels.iter().any(|l| l == "help wanted") {
        label_points += 10.0;
        reasons.push("help wanted".to_owned());
    }
    let days = (now - issue.updated_at).num_days();
    let fresh = recency_points(days, &[(7, 15.0), (30, 10.0), (90, 5.0)]);
    reasons.push(format!("updated {days}d ago"));
    let comments = match issue.comments {
        0 => 10.0,
        1..=3 => 7.0,
        4..=10 => 3.0,
        _ => 0.0,
    };
    reasons.push(format!("{} comments", issue.comments));
    (label_points + fresh + comments, reasons)
}

fn human_secs(secs: u64) -> String {
    match secs {
        s if s < 3_600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3_600),
        s => format!("{}d", s / 86_400),
    }
}

/// Metadata-stage elimination: archived, fork, topic mismatch, or no push for [`STALE_DAYS`].
fn metadata_exclusion(
    profile: &RepoProfile,
    query: &DiscoverQuery,
    now: DateTime<Utc>,
) -> Option<String> {
    if profile.archived {
        return Some("archived".to_owned());
    }
    if profile.fork {
        return Some("fork".to_owned());
    }
    if let Some(topic) = query.topic.as_deref().map(str::trim) {
        if !profile.topics.iter().any(|t| t.eq_ignore_ascii_case(topic)) {
            return Some(format!("no topic {topic:?}"));
        }
    }
    profile
        .pushed_at
        .map(|at| (now - at).num_days())
        .filter(|days| *days > STALE_DAYS)
        .map(|days| format!("not pushed for {days} days"))
}

/// Activity-stage elimination:no maintainer activity for [`STALE_DAYS`], or
/// no external PR merged out of at least [`MIN_MERGE_SAMPLE`].
fn activity_exclusion(activity: &RepoActivity, now: DateTime<Utc>) -> Option<String> {
    if let Some(at) = activity.maintainer_last_active {
        let days = (now - at).num_days();
        if days > STALE_DAYS {
            return Some(format!("no maintainer activity for {days} days"));
        }
    }
    if activity.external_pr_merge_rate == Some(0.0) && activity.sample_size >= MIN_MERGE_SAMPLE {
        return Some(format!(
            "none of the last {} external PRs was merged",
            activity.sample_size
        ));
    }
    None
}

fn sort_candidates(candidates: &mut [Candidate]) {
    candidates.sort_by(|a, b| {
        let shortlist = |c: &Candidate| c.mark.as_ref().is_some_and(CandidateMark::is_shortlist);
        shortlist(b)
            .cmp(&shortlist(a))
            .then(b.score.total_cmp(&a.score))
            .then(b.issue.updated_at.cmp(&a.issue.updated_at))
            .then(a.issue.issue.cmp(&b.issue.issue))
    });
}

fn make_candidate(
    row: IssueRow,
    profile: Option<&RepoProfile>,
    mark: Option<&StoredIssueMark>,
    now: DateTime<Utc>,
) -> Candidate {
    let (repo_points, mut reasons) = profile.map_or((0.0, Vec::new()), |p| repo_score(p, now));
    let (issue_points, issue_reasons) = issue_score(&row, now);
    reasons.extend(issue_reasons);
    let mark = mark.map(|m| CandidateMark::from_stored(m, &row));
    if let Some(mark) = mark.as_ref().filter(|m| m.is_shortlist()) {
        reasons.insert(0, format!("shortlisted: {}", mark.note));
    }
    Candidate {
        issue: row,
        score: ((repo_points + issue_points) * 10.0).round() / 10.0,
        reasons,
        mark,
        claim: None,
    }
}

/// Every label in `wanted` is on the issue, compared case-insensitively.
fn has_all_labels(row: &IssueRow, wanted: &[String]) -> bool {
    wanted.iter().all(|want| {
        let want = want.trim().to_lowercase();
        row.labels.iter().any(|l| l.trim().to_lowercase() == want)
    })
}

impl Pipeline {
    /// The real pipeline: [`GitHubProfiler`] and [`GitHubIndexer`] on `gateway`.
    pub fn new(gateway: Arc<Gateway>, store: SharedStore, config: Config) -> Self {
        let indexer = Arc::new(GitHubIndexer {
            gateway: gateway.clone(),
            store: store.clone(),
            config: config.clone(),
        });
        Self {
            profiler: GitHubProfiler::new(gateway.clone(), store.clone(), config.clone()),
            gateway,
            store,
            config,
            indexer,
        }
    }

    /// Replaces the issue indexer (tests use a fake).
    pub fn with_indexer(mut self, indexer: Arc<dyn IssueIndexer>) -> Self {
        self.indexer = indexer;
        self
    }

    /// The cached run if usable in `mode` without a request.
    fn usable_run(
        &self,
        q: &str,
        mode: Freshness,
        now: DateTime<Utc>,
    ) -> Result<Option<DiscoverRun>> {
        let run = lock(&self.store).discover_run(&run_key(q))?;
        Ok(run.and_then(|run| {
            let usable = mode == Freshness::Offline
                || !mode.needs_fetch(Some(age(run.fetched_at, now)), self.config.ttls().search);
            usable.then_some(run.data)
        }))
    }

    fn marks_for(&self, repos: &[RepoName]) -> Result<HashMap<String, PolicyMark>> {
        Ok(lock(&self.store)
            .effective_marks(repos)?
            .into_iter()
            .map(|(repo, mark)| (repo.key(), mark))
            .collect())
    }
}

/// Mutable state of one discover run.
struct Run {
    report: DiscoverReport,
    stopped: bool,
}

impl Run {
    fn exclude(&mut self, repo: &RepoName, stage: Stage, reason: impl Into<String>) {
        self.report.excluded.push(Exclusion {
            repo: repo.clone(),
            stage,
            reason: reason.into(),
        });
    }

    fn warn(&mut self, warning: String) {
        self.report.partial = true;
        if !self.report.warnings.contains(&warning) {
            self.report.warnings.push(warning);
        }
    }

    /// Records a failed stage; returns true when the run must stop.
    fn failed(&mut self, repo: &RepoName, stage: Stage, error: &anyhow::Error) -> bool {
        if stops_run(error) {
            self.warn(format!(
                "stopped at {repo} ({stage:?} stage): {error:#}; remaining repositories were not checked"
            ));
            self.stopped = true;
            return true;
        }
        self.warn(format!("{repo}: {stage:?} stage failed: {error:#}"));
        false
    }
}

impl Discovery for Pipeline {
    fn discover(&self, query: &DiscoverQuery, mode: Freshness) -> Result<DiscoverReport> {
        let now = Utc::now();
        let q = search_query(query);
        let mut run = Run {
            report: DiscoverReport {
                query: q.clone(),
                repos: Vec::new(),
                excluded_marked: 0,
                excluded_skipped: 0,
                excluded: Vec::new(),
                new_repos: 0,
                reminders: Vec::new(),
                warnings: Vec::new(),
                partial: false,
            },
            stopped: false,
        };

        // Stage 1: search (one page, cached in `discover_runs`).
        let repos = match self.usable_run(&q, mode, now)? {
            Some(cached) => cached.repos,
            None => {
                let previous = lock(&self.store).discover_run(&run_key(&q))?;
                let request = ApiRequest::get(search_path(&q));
                let fetched = self
                    .gateway
                    .get(&request, mode, self.config.ttls().search)?;
                if let Some(note) = &fetched.stale {
                    run.warn(note.clone());
                }
                let body: Value = fetched
                    .response
                    .json_body()
                    .context("parse issue search response")?;
                if body.get("incomplete_results").and_then(Value::as_bool) == Some(true) {
                    run.warn("GitHub reported incomplete search results".to_owned());
                }
                let repos = repos_from_search(&body);
                if let Some(previous) = previous {
                    let known: HashSet<String> =
                        previous.data.repos.iter().map(RepoName::key).collect();
                    run.report.new_repos = repos
                        .iter()
                        .filter(|r| !known.contains(&r.key()))
                        .count()
                        .try_into()
                        .unwrap_or(u32::MAX);
                }
                let saved = DiscoverRun {
                    query_key: run_key(&q),
                    query: q.clone(),
                    params: json!({
                        "language": query.language,
                        "topic": query.topic,
                        "labels": query.effective_labels(),
                        "per_page": SEARCH_PER_PAGE,
                    }),
                    repos: repos.clone(),
                };
                lock(&self.store).save_discover_run(&saved, fetched.fetched_at)?;
                repos
            }
        };

        // Stage 2: local policy marks for every repository (zero requests).
        let marks = self.marks_for(&repos)?;
        let issue_marks: HashMap<String, StoredIssueMark> = lock(&self.store)
            .active_issue_marks(now)?
            .into_iter()
            .map(|m| (m.issue.to_string().to_ascii_lowercase(), m))
            .collect();
        let wanted_labels: Vec<String> = query
            .effective_labels()
            .iter()
            .map(|l| normalize_label(l))
            .collect();
        let limit = usize::try_from(query.limit.max(1)).unwrap_or(usize::MAX);

        for repo in &repos {
            if run.report.repos.len() >= limit || run.stopped {
                break;
            }
            let mark = marks.get(&repo.key());
            run.report.reminders.push(reminder_for(repo, mark, now));
            let bans = is_bans(mark);
            if bans && !query.include_marked {
                run.report.excluded_marked += 1;
                let note = mark.map(|m| m.note.as_str()).unwrap_or_default();
                run.exclude(repo, Stage::PolicyMark, format!("marked Bans: {note}"));
                continue;
            }

            // Stage 3: metadata.
            let mut profile = match self.profiler.profile(repo, mode) {
                Ok(profile) => profile,
                Err(error) => {
                    if !run.failed(repo, Stage::Metadata, &error) {
                        run.exclude(repo, Stage::Metadata, format!("{error:#}"));
                    }
                    continue;
                }
            };
            for warning in self.profiler.take_warnings() {
                run.warn(warning);
            }
            if let Some(reason) = metadata_exclusion(&profile, query, now) {
                run.exclude(repo, Stage::Metadata, reason);
                continue;
            }

            // Stage 4: activity (never for Bans repos, §7).
            if !bans {
                match self.profiler.activity(repo, mode) {
                    Ok(activity) => {
                        let excluded = activity_exclusion(&activity, now);
                        profile.activity = Some(activity);
                        if let Some(reason) = excluded {
                            run.exclude(repo, Stage::Activity, reason);
                            continue;
                        }
                    }
                    // Offline without cached activity: score without it.
                    Err(error) if mode == Freshness::Offline && is_not_found(&error) => {}
                    Err(error) => {
                        if run.failed(repo, Stage::Activity, &error) {
                            break;
                        }
                    }
                }
            }

            // Stage 5: issue index.
            if let Err(error) = self.indexer.sync(repo, mode) {
                if run.failed(repo, Stage::Index, &error) {
                    break;
                }
            }
            let rows = lock(&self.store).open_unassigned_issues(std::slice::from_ref(repo))?;
            let mut candidates = Vec::new();
            for indexed in rows {
                if indexed.is_pull_request
                    || !indexed
                        .row
                        .labels
                        .iter()
                        .any(|l| wanted_labels.contains(&normalize_label(l)))
                {
                    continue;
                }
                let mark = issue_marks.get(&indexed.row.issue.to_string().to_ascii_lowercase());
                if mark.is_some_and(StoredIssueMark::is_skip) && !query.include_marked {
                    run.report.excluded_skipped += 1;
                    continue;
                }
                candidates.push(make_candidate(indexed.row, Some(&profile), mark, now));
            }
            if candidates.is_empty() {
                run.exclude(
                    repo,
                    Stage::Index,
                    format!(
                        "no indexed open, unassigned issue labelled {}",
                        query.effective_labels().join(" or ")
                    ),
                );
                continue;
            }

            // Stage 6: local scoring.
            sort_candidates(&mut candidates);
            candidates.truncate(CANDIDATES_PER_REPO);
            let score = candidates.first().map_or(0.0, |c| c.score);
            run.report.repos.push(DiscoveredRepo {
                profile,
                score,
                candidates,
            });
        }

        run.report.repos.sort_by(|a, b| b.score.total_cmp(&a.score));
        Ok(run.report)
    }

    fn plan(&self, query: &DiscoverQuery, mode: Freshness) -> Result<DiscoverPlan> {
        let now = Utc::now();
        let q = search_query(query);
        let cached = self.usable_run(&q, mode, now)?;
        let offline = mode == Freshness::Offline;
        let limit = usize::try_from(query.limit.max(1)).unwrap_or(usize::MAX);
        let search_cached = cached.is_some();
        let search_requests = u32::from(!search_cached && !offline);

        let mut reminders = Vec::new();
        let (mut profile_requests, mut activity_requests, mut index_requests) = (0u32, 0u32, 0u32);
        let mut excluded_marked = 0u32;
        let known_repos = cached.map(|run| run.repos).unwrap_or_default();
        if known_repos.is_empty() {
            if !offline {
                let n = u32::try_from(limit).unwrap_or(u32::MAX);
                (profile_requests, activity_requests, index_requests) = (n, n, n);
            }
        } else {
            let marks = self.marks_for(&known_repos)?;
            let mut kept = 0usize;
            for repo in &known_repos {
                if kept >= limit {
                    break;
                }
                let mark = marks.get(&repo.key());
                reminders.push(reminder_for(repo, mark, now));
                let bans = is_bans(mark);
                if bans && !query.include_marked {
                    excluded_marked += 1;
                    continue;
                }
                if offline {
                    kept += 1;
                    continue;
                }
                if self.profiler.profile_is_cached(repo, mode)? {
                    // A cached profile that fails the metadata stage costs nothing more.
                    let cached = lock(&self.store).repo(repo)?;
                    if cached
                        .is_some_and(|(c, _)| metadata_exclusion(&c.data, query, now).is_some())
                    {
                        continue;
                    }
                } else {
                    profile_requests += 1;
                }
                kept += 1;
                if !bans {
                    activity_requests += u32::from(!self.profiler.activity_is_cached(repo, mode)?);
                }
                index_requests += 1;
            }
        }

        let stages = vec![
            PlanStage {
                stage: Stage::Search,
                description: format!(
                    "issue search, one page of {SEARCH_PER_PAGE} ({})",
                    if search_requests == 0 {
                        "cached run"
                    } else {
                        "cached run missing or older than search_ttl"
                    }
                ),
                estimated_requests: search_requests,
            },
            PlanStage {
                stage: Stage::PolicyMark,
                description: format!(
                    "local policy marks, zero requests{}",
                    if query.include_marked {
                        String::new()
                    } else {
                        format!("; Bans repos excluded ({excluded_marked} known)")
                    }
                ),
                estimated_requests: 0,
            },
            PlanStage {
                stage: Stage::Metadata,
                description: "GET /repos/{owner}/{repo} per uncached repo; drops archived, forks, topic mismatches, stale pushes".into(),
                estimated_requests: profile_requests,
            },
            PlanStage {
                stage: Stage::Activity,
                description: "one GraphQL query per repo without fresh activity (not for Bans repos)".into(),
                estimated_requests: activity_requests,
            },
            PlanStage {
                stage: Stage::Index,
                description: "issue index sync per kept repo (upper bound; the indexer's own TTL may skip it)".into(),
                estimated_requests: index_requests,
            },
            PlanStage {
                stage: Stage::Score,
                description: "local scoring, zero requests".into(),
                estimated_requests: 0,
            },
        ];
        let estimated_requests = stages.iter().map(|s| s.estimated_requests).sum();
        Ok(DiscoverPlan {
            search_path: search_path(&q),
            query: q,
            search_cached,
            known_repos,
            stages,
            estimated_requests,
            reminders,
        })
    }

    fn candidates(&self, query: &CandidateQuery) -> Result<CandidatesReport> {
        let now = Utc::now();
        let store = lock(&self.store);
        let rows = store.open_unassigned_issues(&query.repos)?;
        let issue_marks: HashMap<String, StoredIssueMark> = store
            .active_issue_marks(now)?
            .into_iter()
            .map(|m| (m.issue.to_string().to_ascii_lowercase(), m))
            .collect();
        // Same "active" definition as the claim store and `gate start`:
        // status active and `lease_until > now`. Should one issue ever carry
        // several, a claim held by someone else wins (it is the one that hides).
        let caller = query.agent.as_ref();
        let mut claims: HashMap<String, Claim> = HashMap::new();
        for claim in store.claims(None, now)? {
            let key = claim.issue.to_string().to_ascii_lowercase();
            let other = caller != Some(&claim.agent);
            match claims.get(&key) {
                Some(kept) if !other || caller != Some(&kept.agent) => {}
                _ => {
                    claims.insert(key, claim);
                }
            }
        }
        let mut repos: BTreeMap<String, RepoName> = BTreeMap::new();
        for indexed in &rows {
            let repo = &indexed.row.issue.repo;
            repos.entry(repo.key()).or_insert_with(|| repo.clone());
        }
        let repo_list: Vec<RepoName> = repos.values().cloned().collect();
        let marks: HashMap<String, PolicyMark> = store
            .effective_marks(&repo_list)?
            .into_iter()
            .map(|(repo, mark)| (repo.key(), mark))
            .collect();
        let mut profiles: HashMap<String, RepoProfile> = HashMap::new();
        for repo in &repo_list {
            if let Some((cached, _)) = store.repo(repo)? {
                let mut profile = cached.data;
                profile.activity = store.activity(repo)?;
                profiles.insert(repo.key(), profile);
            }
        }
        drop(store);

        let mut report = CandidatesReport {
            candidates: Vec::new(),
            excluded_marked: 0,
            excluded_skipped: 0,
            excluded_claimed: 0,
            reminders: Vec::new(),
        };
        let mut remind: BTreeMap<String, RepoName> = BTreeMap::new();
        for indexed in rows {
            if indexed.is_pull_request || !has_all_labels(&indexed.row, &query.labels) {
                continue;
            }
            let repo = indexed.row.issue.repo.clone();
            let profile = profiles.get(&repo.key());
            if profile.is_some_and(|p| p.archived || p.fork) {
                continue;
            }
            if let Some(language) = query.language.as_deref() {
                let matches = profile
                    .and_then(|p| p.language.as_deref())
                    .is_some_and(|l| l.eq_ignore_ascii_case(language.trim()));
                if !matches {
                    continue;
                }
            }
            if is_bans(marks.get(&repo.key())) && !query.include_marked {
                report.excluded_marked += 1;
                remind.insert(repo.key(), repo);
                continue;
            }
            let mark = issue_marks.get(&indexed.row.issue.to_string().to_ascii_lowercase());
            if mark.is_some_and(StoredIssueMark::is_skip) && !query.include_marked {
                report.excluded_skipped += 1;
                continue;
            }
            let claim = claims.get(&indexed.row.issue.to_string().to_ascii_lowercase());
            let mut candidate = make_candidate(indexed.row, profile, mark, now);
            let shortlisted = candidate
                .mark
                .as_ref()
                .is_some_and(CandidateMark::is_shortlist);
            if query.min_score.is_some_and(|min| candidate.score < min) && !shortlisted {
                continue;
            }
            if let Some(claim) = claim {
                if caller != Some(&claim.agent) && !query.include_claimed {
                    report.excluded_claimed += 1;
                    continue;
                }
                candidate.claim = Some(CandidateClaim {
                    agent: claim.agent.clone(),
                    lease_until: claim.lease_until,
                });
            }
            report.candidates.push(candidate);
        }
        sort_candidates(&mut report.candidates);
        report
            .candidates
            .truncate(usize::try_from(query.limit.max(1)).unwrap_or(usize::MAX));
        for candidate in &report.candidates {
            let repo = &candidate.issue.issue.repo;
            remind.insert(repo.key(), repo.clone());
        }
        report.reminders = remind
            .values()
            .map(|repo| reminder_for(repo, marks.get(&repo.key()), now))
            .collect();
        Ok(report)
    }
}
