//! Incremental issue index (§1, §6, §7). Step 5 owns this file.
//!
//! # Sync
//! One pass lists `GET /repos/{o}/{r}/issues?sort=updated&direction=asc&per_page=100`
//! until the end or until the request budget / shared quota / network stops
//! it. PRs (items with a `pull_request` key) are skipped. Only index fields
//! are stored; bodies are fetched on demand.
//!
//! - The first pass of a repository lists `state=open` without `since` (no full
//!   history scan of large repositories, §1). Later passes list `state=all` with
//!   `since = watermark - index_overlap`, so closures and edits are picked up.
//! - Paging is keyset-style, not by page number. Page numbers over
//!   `sort=updated` shift while the pass runs: an item updated mid-pass moves to
//!   the end, every later item moves one slot up, and the item that slid onto an
//!   already-read page would be skipped for good. So after each page the next
//!   request is page 1 again with `since = max updated_at seen` (inclusive, so
//!   items at that exact time come back and are deduplicated by number and
//!   `updated_at`). A page-1 answer is always a consistent prefix of the
//!   remaining items. Only when a page makes no progress (no item newer than
//!   the `since` it was asked with, e.g. more than a page of items share one
//!   timestamp) is its `Link: rel="next"` page followed instead; such a pass
//!   can skip items again, so its watermark never advances past the one the
//!   pass started from (on a first pass: the `since` of that request).
//! - Because every keyset page is a prefix in ascending `updated_at` order,
//!   after any page every item updated before the largest `updated_at` seen so
//!   far is stored. An interrupted pass therefore checkpoints `since = max
//!   updated_at seen` (never moving backwards) and is marked `partial`; the
//!   next run resumes from there and ignores the TTL.
//! - A completed pass sets the watermark to the time its earliest run
//!   *started* (local clock), unless it had to follow `Link` pages (above). Anything updated while the pass was running, or
//!   while it was interrupted, is newer than that and is listed again by the
//!   next pass; `index_overlap` (default 10m) absorbs clock skew between this
//!   machine and GitHub. Using the start time rather than the newest
//!   `updated_at` seen keeps `since` tight for quiet repositories.
//! - `Default` sends nothing when the last complete pass started within
//!   `issue_index_ttl`; `Fresh` ignores the TTL; `Offline` never sends.
//! - One process syncs a repository at a time (`Store::lock_scope`); a second
//!   one waits up to `lock_wait` and then re-checks the TTL.
//! - List pages are sent uncached (`Gateway::send`): their `since` changes on
//!   every pass, so conditional caching would only grow `http_cache`.
//!
//! # Details
//! [`GitHubIndexer::fetch_issue`] reads the issue, its last `gate_comments`
//! comments, and its timeline (for linked PRs) through conditional, cached
//! requests (`issue_detail_ttl`). [`IssueIndexer::issue_live`] reads the same
//! with `Gateway::get_live` for gates. Timeline `Link` pages must stay on
//! `https://api.github.com` and end in `/timeline` (dwave `safe_api_url`);
//! anything else is refused, since a gate cannot vouch for linked PRs it did not read.

use crate::config::Config;
use crate::error::{Result, ScoutError};
use crate::freshness::{Freshness, Live, LiveRead, Origin};
use crate::gateway::Gateway;
use crate::models::{
    IssueComment, IssueDetail, IssueRef, IssueRow, IssueState, LinkedPr, RepoName,
};
use crate::store::{SharedStore, Store, SyncState};
use crate::transport::{ApiRequest, ApiResponse};
use anyhow::{anyhow, bail, Context};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, MutexGuard, PoisonError};
use std::time::Duration;

/// Why a sync sent no requests.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncSkip {
    /// `--offline`.
    Offline,
    /// The last complete pass is younger than `issue_index_ttl`.
    Fresh,
    /// Another process held the repository's sync lock past `lock_wait`.
    Locked,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncReport {
    pub repo: RepoName,
    /// Issues written because they are new or their index fields changed.
    pub updated: u32,
    /// Issues seen for the first time — with `updated`, drives exit code 10.
    pub new_issues: u32,
    /// Issues listed again with nothing new (overlap window).
    #[serde(default)]
    pub unchanged: u32,
    /// Pull requests in the listing, not indexed.
    #[serde(default)]
    pub skipped_prs: u32,
    /// The `since` sent (watermark minus overlap); `None` on the first pass.
    pub since: Option<DateTime<Utc>>,
    /// Budget, quota, or network stopped the sync early; the watermark only
    /// covers what was stored, and the next run resumes from it.
    pub partial: bool,
    /// The first pass (`state=open`, no `since`) rather than an incremental one.
    #[serde(default)]
    pub first_pass: bool,
    #[serde(default)]
    pub pages: u32,
    /// Network requests sent for this repository.
    #[serde(default)]
    pub requests: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped: Option<SyncSkip>,
    /// Watermark after this run.
    #[serde(default)]
    pub watermark: Option<DateTime<Utc>>,
    #[serde(default)]
    pub warnings: Vec<String>,
}

impl SyncReport {
    /// An empty report for `repo` (no requests, no changes).
    pub fn new(repo: &RepoName) -> Self {
        Self {
            repo: repo.clone(),
            updated: 0,
            new_issues: 0,
            unchanged: 0,
            skipped_prs: 0,
            since: None,
            partial: false,
            first_pass: false,
            pages: 0,
            requests: 0,
            skipped: None,
            watermark: None,
            warnings: Vec::new(),
        }
    }

    /// Whether new or changed issues were indexed (§10 exit code 10).
    pub fn has_changes(&self) -> bool {
        self.updated > 0
    }
}

pub trait IssueIndexer {
    /// Refreshes one repository's index according to `mode` (TTL 1h by default).
    fn sync(&self, repo: &RepoName, mode: Freshness) -> Result<SyncReport>;

    /// Issue with body, recent comments, and timeline PRs (TTL 30m, `issue show`).
    fn issue(&self, issue: &IssueRef, mode: Freshness) -> Result<IssueDetail>;

    /// Gate variant: always live (§8).
    fn issue_live(&self, issue: &IssueRef, live: LiveRead) -> Result<Live<IssueDetail>>;
}

/// An issue detail read in a cached mode, with what the envelope needs.
#[derive(Clone, Debug, PartialEq)]
pub struct IssueFetch {
    pub detail: IssueDetail,
    /// Oldest confirmation time among the responses used.
    pub as_of: DateTime<Utc>,
    /// Stale-cache notes; non-empty means `meta.partial`.
    pub warnings: Vec<String>,
}

pub struct GitHubIndexer {
    pub gateway: Arc<Gateway>,
    pub store: SharedStore,
    pub config: Config,
}

#[derive(Clone, Copy)]
enum Read {
    Cached(Freshness),
    Live(LiveRead),
}

struct Page {
    response: ApiResponse,
    fetched_at: DateTime<Utc>,
    origin: Origin,
}

struct DetailRead {
    detail: IssueDetail,
    as_of: DateTime<Utc>,
    all_not_modified: bool,
    warnings: Vec<String>,
}

/// GitHub timestamps (`2026-11-25T10:00:00Z`).
fn parse_time(value: &Value) -> Option<DateTime<Utc>> {
    value
        .as_str()
        .and_then(|text| DateTime::parse_from_rfc3339(text).ok())
        .map(|time| time.with_timezone(&Utc))
}

fn github_time(time: DateTime<Utc>) -> String {
    time.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Index fields of one issue from GitHub's REST JSON.
pub fn parse_issue_row(
    repo: &RepoName,
    value: &Value,
    fetched_at: DateTime<Utc>,
) -> Result<IssueRow> {
    let number = value["number"]
        .as_u64()
        .ok_or_else(|| anyhow!("issue in {repo} without a number"))?;
    let updated_at = parse_time(&value["updated_at"])
        .ok_or_else(|| anyhow!("issue {repo}#{number} without updated_at"))?;
    let strings = |key: &str, field: &str| -> Vec<String> {
        value[key]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|item| item[field].as_str().or_else(|| item.as_str()))
            .map(str::to_owned)
            .collect()
    };
    let mut assignees: BTreeSet<String> = strings("assignees", "login").into_iter().collect();
    if let Some(login) = value["assignee"]["login"].as_str() {
        assignees.insert(login.to_owned());
    }
    Ok(IssueRow {
        issue: IssueRef::new(repo.clone(), number),
        title: value["title"].as_str().unwrap_or_default().to_owned(),
        state: IssueState::from_stored(value["state"].as_str().unwrap_or("open")),
        labels: strings("labels", "name"),
        assignees: assignees.into_iter().collect(),
        comments: value["comments"]
            .as_u64()
            .and_then(|c| u32::try_from(c).ok())
            .unwrap_or(0),
        created_at: parse_time(&value["created_at"]),
        updated_at,
        fetched_at,
    })
}

/// The `rel="next"` target of a `Link` header (dwave `next_link`).
pub fn next_link(header: &str) -> Option<&str> {
    header.split(',').find_map(|part| {
        let (url, parameters) = part.trim().strip_prefix('<')?.split_once('>')?;
        parameters
            .split(';')
            .any(|parameter| {
                parameter
                    .trim()
                    .split_once('=')
                    .is_some_and(|(key, value)| {
                        key.trim() == "rel"
                            && value
                                .trim()
                                .trim_matches('"')
                                .split_whitespace()
                                .any(|rel| rel == "next")
                    })
            })
            .then_some(url)
    })
}

/// A pagination link we are willing to follow: plain `https://api.github.com`,
/// no credentials, port, or fragment, and a path ending in `suffix`.
pub fn safe_api_url(url: &str, suffix: &str) -> bool {
    reqwest::Url::parse(url).is_ok_and(|url| {
        url.scheme() == "https"
            && url.host_str() == Some("api.github.com")
            && url.port().is_none()
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none()
            && url.path().ends_with(suffix)
    })
}

/// Timeline `cross-referenced` / `connected` events add a PR, `disconnected`
/// removes it (adapted from dwave `apply_pr_event`), but only while the PR is
/// linked through the Development sidebar alone: `disconnected` undoes a
/// `connected` link, never a cross-reference (a mention in a PR body or
/// commit stays), so once a PR has cross-referenced the issue its `event`
/// stays `cross-referenced` in any order of events. Later events fill in an
/// author or state but never erase one.
///
/// Two shapes occur. `cross-referenced` carries the PR as an issue object in
/// `source.issue` (with a `pull_request` key, `html_url`, `user`). Real
/// `connected` / `disconnected` events (Development sidebar links) carry a
/// bare `subject` instead: `{type: "pull_request", number, url (API),
/// state, merged_at, repository: {full_name, ...}}`, no `user`, no
/// `html_url`. Sidebar links can also be absent from the timeline entirely,
/// which is why gates add the GraphQL closing references
/// (`closedByPullRequestsReferences`) on top.
pub fn apply_pr_event(event: &Value, prs: &mut BTreeMap<String, LinkedPr>) {
    let Some(found) = event_pr(event) else {
        return;
    };
    match event["event"].as_str() {
        Some(name @ ("cross-referenced" | "connected")) => {
            let pr = prs.entry(found.url.clone()).or_insert_with(|| LinkedPr {
                url: found.url,
                event: name.to_owned(),
                author: None,
                state: None,
                head_ref: None,
            });
            if name == "cross-referenced" {
                pr.event = name.to_owned();
            }
            if found.author.is_some() {
                pr.author = found.author;
            }
            if found.state.is_some() {
                pr.state = found.state;
            }
        }
        Some("disconnected")
            if prs
                .get(&found.url)
                .is_some_and(|pr| pr.event == "connected") =>
        {
            prs.remove(&found.url);
        }
        _ => {}
    }
}

struct EventPr {
    url: String,
    author: Option<String>,
    state: Option<String>,
}

fn event_pr(event: &Value) -> Option<EventPr> {
    let source = &event["source"]["issue"];
    let target = &event["subject"];
    let as_issue = [source, target].into_iter().find_map(|issue| {
        let pr = issue.get("pull_request")?.as_object()?;
        let url = pr
            .get("html_url")
            .and_then(Value::as_str)
            .or_else(|| issue["html_url"].as_str())?;
        let merged = pr.get("merged_at").is_some_and(|m| !m.is_null());
        Some(EventPr {
            url: url.to_owned(),
            author: issue["user"]["login"].as_str().map(str::to_owned),
            state: if merged {
                Some("merged".into())
            } else {
                issue["state"].as_str().map(str::to_owned)
            },
        })
    });
    as_issue.or_else(|| {
        // The bare `subject` of `connected` / `disconnected`.
        if target["type"].as_str() != Some("pull_request") {
            return None;
        }
        let number = target["number"].as_u64()?;
        let repo = target["repository"]["full_name"].as_str()?;
        let merged = target.get("merged_at").is_some_and(|m| !m.is_null());
        Some(EventPr {
            url: format!("https://github.com/{repo}/pull/{number}"),
            author: None,
            state: if merged {
                Some("merged".into())
            } else {
                target["state"].as_str().map(str::to_ascii_lowercase)
            },
        })
    })
}

/// Errors after which a sync keeps what it stored and reports `partial`.
fn resumable(error: &anyhow::Error) -> bool {
    match ScoutError::find(error) {
        Some(
            ScoutError::BudgetExhausted { .. }
            | ScoutError::QuotaReserved { .. }
            | ScoutError::WindowLimit { .. }
            | ScoutError::NetworkUnavailable { .. },
        ) => true,
        Some(ScoutError::HttpStatus { status, .. }) => {
            matches!(status, 403 | 429) || *status >= 500
        }
        _ => false,
    }
}

impl GitHubIndexer {
    pub fn new(gateway: Arc<Gateway>, store: SharedStore, config: Config) -> Self {
        Self {
            gateway,
            store,
            config,
        }
    }

    fn store(&self) -> MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn within_ttl(state: &SyncState, now: DateTime<Utc>, ttl: Duration) -> bool {
        !state.partial
            && state
                .last_sync_at
                .is_some_and(|at| (now - at).to_std().is_ok_and(|age| age < ttl))
    }

    fn sync_repo(&self, repo: &RepoName, mode: Freshness) -> Result<SyncReport> {
        let ttls = self.config.ttls();
        let mut report = SyncReport::new(repo);
        let started = Utc::now();
        let state = self.store().sync_state(repo)?;
        report.watermark = state.since;
        if !mode.allows_fetch() {
            report.skipped = Some(SyncSkip::Offline);
            return Ok(report);
        }
        if mode == Freshness::Default && Self::within_ttl(&state, started, ttls.issue_index) {
            report.skipped = Some(SyncSkip::Fresh);
            return Ok(report);
        }

        // One syncer per repository across processes. The guard owns a file
        // handle, not the store mutex, so the gateway can still use the store.
        let scope = format!("sync {}", repo.key());
        let guard = self.store().lock_scope(&scope, self.config.lock_wait())?;
        let Some(_guard) = guard else {
            report.skipped = Some(SyncSkip::Locked);
            report
                .warnings
                .push(format!("another agent is syncing {repo}; skipped"));
            return Ok(report);
        };
        // Another process may have finished a pass while we waited.
        let state = self.store().sync_state(repo)?;
        if mode == Freshness::Default && Self::within_ttl(&state, Utc::now(), ttls.issue_index) {
            report.skipped = Some(SyncSkip::Fresh);
            report.watermark = state.since;
            return Ok(report);
        }

        let first_pass = state.last_full_at.is_none();
        let since = state.since.map(|since| {
            since - chrono::TimeDelta::from_std(ttls.index_overlap).unwrap_or_default()
        });
        report.first_pass = first_pass;
        report.since = since;
        let pass_started = state
            .pass_started_at
            .filter(|_| state.partial)
            .unwrap_or(started);
        let base = format!(
            "/repos/{}/{}/issues?state={}&sort=updated&direction=asc&per_page=100",
            repo.owner(),
            repo.name(),
            if first_pass { "open" } else { "all" }
        );
        let keyed = |since: Option<DateTime<Utc>>| match since {
            Some(since) => format!("{base}&since={}", github_time(since)),
            None => base.clone(),
        };

        let requests_before = self.gateway.metrics().requests;
        let mut watermark = state.since;
        // Set once a page made no progress and its `Link` page was followed:
        // page numbers can skip items, so the watermark stays at the pass's
        // starting one (or the `since` of that request when there was none).
        let mut capped: Option<Option<DateTime<Utc>>> = None;
        let mut cursor = since;
        // Issue number -> `updated_at` already stored by this pass.
        let mut seen: BTreeMap<u64, DateTime<Utc>> = BTreeMap::new();
        let mut visited = BTreeSet::new();
        let mut next = Some(keyed(cursor));
        let mut complete = false;
        while let Some(url) = next.take() {
            if url.starts_with("https://") && !safe_api_url(&url, "/issues") {
                report.partial = true;
                report
                    .warnings
                    .push(format!("refused a pagination link outside the API: {url}"));
                break;
            }
            if !visited.insert(url.clone()) {
                report.partial = true;
                report
                    .warnings
                    .push(format!("pagination loop at {url}; stopped"));
                break;
            }
            let response = match self.gateway.send(&ApiRequest::get(url.as_str())) {
                Ok(response) => response,
                Err(error) if resumable(&error) => {
                    report.partial = true;
                    report
                        .warnings
                        .push(format!("sync of {repo} stopped early: {error:#}"));
                    break;
                }
                Err(error) => return Err(error),
            };
            if response.status == 404 {
                return Err(ScoutError::NotFound {
                    what: format!("repository {repo}"),
                }
                .into());
            }
            let items: Vec<Value> = response
                .json_body()
                .with_context(|| format!("issue listing of {repo}"))?;
            let fetched_at = Utc::now();
            let mut rows = Vec::new();
            let mut page_max: Option<DateTime<Utc>> = None;
            for item in &items {
                let updated = parse_time(&item["updated_at"]);
                if let Some(updated) = updated {
                    page_max = Some(page_max.map_or(updated, |m| m.max(updated)));
                }
                // `since` is inclusive: skip what this pass already stored.
                if let (Some(number), Some(updated)) = (item["number"].as_u64(), updated) {
                    if seen.insert(number, updated) == Some(updated) {
                        continue;
                    }
                }
                if item.get("pull_request").is_some() {
                    report.skipped_prs += 1;
                    continue;
                }
                rows.push((parse_issue_row(repo, item, fetched_at)?, item.clone()));
            }
            if let Some(max) = page_max {
                watermark = Some(watermark.map_or(max, |w| w.max(max)));
            }
            report.pages += 1;
            next = match response.header_value("link").and_then(next_link) {
                None => None,
                Some(link) => match page_max {
                    // Progress: restart at page 1 from the newest time seen.
                    Some(max) if cursor.is_none_or(|c| max > c) => {
                        cursor = Some(max);
                        Some(keyed(cursor))
                    }
                    // A full page at one timestamp: only page numbers move on.
                    _ => {
                        capped.get_or_insert(state.since.or(cursor));
                        Some(link.to_owned())
                    }
                },
            };
            let checkpoint = SyncState {
                since: capped.unwrap_or(watermark),
                last_full_at: state.last_full_at,
                last_sync_at: state.last_sync_at,
                partial: true,
                pass_started_at: Some(pass_started),
            };
            let stats = {
                let mut store = self.store();
                let stats = store.save_issues(&rows)?;
                if next.is_some() {
                    store.save_sync_state(repo, &checkpoint)?;
                }
                stats
            };
            report.new_issues += stats.new;
            report.updated += stats.new + stats.changed;
            report.unchanged += stats.unchanged;
            if next.is_none() {
                complete = true;
            }
        }

        let final_state = if complete {
            SyncState {
                since: capped.unwrap_or(Some(pass_started)),
                last_full_at: if first_pass {
                    Some(started)
                } else {
                    state.last_full_at
                },
                last_sync_at: Some(started),
                partial: false,
                pass_started_at: None,
            }
        } else {
            report.partial = true;
            SyncState {
                since: capped.unwrap_or(watermark),
                last_full_at: state.last_full_at,
                last_sync_at: Some(started),
                partial: true,
                pass_started_at: Some(pass_started),
            }
        };
        self.store().save_sync_state(repo, &final_state)?;
        report.watermark = final_state.since;
        report.requests = self.gateway.metrics().requests - requests_before;
        Ok(report)
    }

    fn read(&self, path: &str, read: Read, ttl: Duration) -> Result<(Page, Option<String>)> {
        let request = ApiRequest::get(path);
        match read {
            Read::Cached(freshness) => {
                let fetched = self.gateway.get(&request, freshness, ttl)?;
                Ok((
                    Page {
                        response: fetched.response,
                        fetched_at: fetched.fetched_at,
                        origin: fetched.origin,
                    },
                    fetched.stale,
                ))
            }
            Read::Live(live) => {
                let live = self.gateway.get_live(&request, live)?;
                let origin = if live.not_modified() {
                    Origin::NotModified
                } else {
                    Origin::Network
                };
                let fetched_at = live.as_of();
                Ok((
                    Page {
                        response: live.into_inner(),
                        fetched_at,
                        origin,
                    },
                    None,
                ))
            }
        }
    }

    fn detail(&self, issue: &IssueRef, read: Read) -> Result<DetailRead> {
        let ttl = self.config.ttls().issue_detail;
        let base = format!(
            "/repos/{}/{}/issues/{}",
            issue.repo.owner(),
            issue.repo.name(),
            issue.number
        );
        let mut as_of: Option<DateTime<Utc>> = None;
        let mut all_not_modified = true;
        let mut warnings = Vec::new();
        let mut note = |page: &Page, stale: Option<String>, warnings: &mut Vec<String>| {
            as_of = Some(as_of.map_or(page.fetched_at, |t| t.min(page.fetched_at)));
            all_not_modified &= page.origin == Origin::NotModified;
            warnings.extend(stale);
        };

        let (page, stale) = self.read(&base, read, ttl)?;
        note(&page, stale, &mut warnings);
        if page.response.status == 404 {
            return Err(ScoutError::NotFound {
                what: format!("issue {issue}"),
            }
            .into());
        }
        let body: Value = page
            .response
            .json_body()
            .with_context(|| format!("issue {issue}"))?;
        let row = parse_issue_row(&issue.repo, &body, page.fetched_at)?;
        if body.get("pull_request").is_none() && page.origin != Origin::Cache {
            // Keep the index current; `save_issues` ignores older copies.
            self.store().save_issues(&[(row.clone(), body.clone())])?;
        }

        // The last `gate_comments` comments: at most the last two pages of size N.
        let wanted = self.config.gate_comments.min(100);
        let mut comments = Vec::new();
        if wanted > 0 && row.comments > 0 {
            let last_page = row.comments.div_ceil(wanted);
            let pages: Vec<u32> = if row.comments % wanted != 0 && last_page > 1 {
                vec![last_page - 1, last_page]
            } else {
                vec![last_page]
            };
            for number in pages {
                let path = format!("{base}/comments?per_page={wanted}&page={number}");
                let (page, stale) = self.read(&path, read, ttl)?;
                note(&page, stale, &mut warnings);
                if page.response.status == 404 {
                    continue;
                }
                let items: Vec<Value> = page
                    .response
                    .json_body()
                    .with_context(|| format!("comments of {issue}"))?;
                comments.extend(items.iter().filter_map(parse_comment));
            }
            let excess = comments.len().saturating_sub(wanted as usize);
            comments.drain(..excess);
        }

        let mut linked = BTreeMap::new();
        let mut next = Some(format!("{base}/timeline?per_page=100"));
        let mut visited = BTreeSet::new();
        while let Some(url) = next.take() {
            if url.starts_with("https://") && !safe_api_url(&url, "/timeline") {
                bail!("refusing a timeline pagination link outside https://api.github.com: {url}");
            }
            if !visited.insert(url.clone()) {
                bail!("timeline pagination loop for {issue} at {url}");
            }
            let (page, stale) = self.read(&url, read, ttl)?;
            note(&page, stale, &mut warnings);
            if page.response.status == 404 {
                return Err(ScoutError::NotFound {
                    what: format!("timeline of {issue}"),
                }
                .into());
            }
            let events: Vec<Value> = page
                .response
                .json_body()
                .with_context(|| format!("timeline of {issue}"))?;
            for event in &events {
                apply_pr_event(event, &mut linked);
            }
            next = page
                .response
                .header_value("link")
                .and_then(next_link)
                .map(str::to_owned);
        }

        Ok(DetailRead {
            detail: IssueDetail {
                author: body["user"]["login"].as_str().map(str::to_owned),
                body: body["body"].as_str().map(str::to_owned),
                comments,
                linked_prs: linked.into_values().collect(),
                is_pull_request: body.get("pull_request").is_some(),
                row,
            },
            as_of: as_of.unwrap_or_else(Utc::now),
            all_not_modified,
            warnings,
        })
    }

    /// [`IssueIndexer::issue`] with the stale-cache warnings and `as_of` the
    /// envelope needs. Also refreshes the issue's index row when the read
    /// reached the network.
    pub fn fetch_issue(&self, issue: &IssueRef, mode: Freshness) -> Result<IssueFetch> {
        let read = match mode.live() {
            Some(live) => Read::Live(live),
            None => Read::Cached(mode),
        };
        let read = self.detail(issue, read)?;
        Ok(IssueFetch {
            detail: read.detail,
            as_of: read.as_of,
            warnings: read.warnings,
        })
    }
}

fn parse_comment(value: &Value) -> Option<IssueComment> {
    Some(IssueComment {
        author: value["user"]["login"].as_str().map(str::to_owned),
        author_association: value["author_association"].as_str().map(str::to_owned),
        body: value["body"].as_str().unwrap_or_default().to_owned(),
        created_at: parse_time(&value["created_at"])?,
        url: value["html_url"].as_str().unwrap_or_default().to_owned(),
    })
}

impl IssueIndexer for GitHubIndexer {
    fn sync(&self, repo: &RepoName, mode: Freshness) -> Result<SyncReport> {
        self.sync_repo(repo, mode)
    }

    fn issue(&self, issue: &IssueRef, mode: Freshness) -> Result<IssueDetail> {
        Ok(self.fetch_issue(issue, mode)?.detail)
    }

    fn issue_live(&self, issue: &IssueRef, live: LiveRead) -> Result<Live<IssueDetail>> {
        let read = self.detail(issue, Read::Live(live))?;
        Ok(Live::new(read.detail, read.as_of, read.all_not_modified))
    }
}
