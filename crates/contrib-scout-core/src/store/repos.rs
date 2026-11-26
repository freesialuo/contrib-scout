//! `repos`, `repo_activity`, `tracked_repos` (§6, §9), plus the read-only
//! queries behind `candidates` over `issues` and `issue_marks` (§5b, §7).
//! Steps 4 (profiles) and 5 (tracking) use this file.

use super::{from_unix, Cached, Store};
use crate::error::Result;
use crate::models::{IssueRef, IssueRow, IssueState, RepoActivity, RepoName, RepoProfile};
use anyhow::Context;
use chrono::{DateTime, Utc};
use rusqlite::{params, OptionalExtension};
use serde_json::Value;

/// An agent-written issue mark as stored in `issue_marks` (§5b), read-only here.
/// Step 5 owns writing marks; discovery and `candidates` only read them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredIssueMark {
    pub issue: IssueRef,
    /// `skip` or `shortlist` (kept verbatim for forward compatibility).
    pub verdict: String,
    pub reason: Option<String>,
    pub note: String,
    pub seen_updated_at: Option<DateTime<Utc>>,
    pub seen_comments: Option<u32>,
    pub marked_by: String,
    pub marked_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
}

impl StoredIssueMark {
    pub fn is_skip(&self) -> bool {
        self.verdict.eq_ignore_ascii_case("skip")
    }

    pub fn is_shortlist(&self) -> bool {
        self.verdict.eq_ignore_ascii_case("shortlist")
    }

    /// The issue's `updated_at` or comment count differ from the snapshot
    /// taken when it was marked (§5b; computed locally, zero requests).
    pub fn changed_since(&self, row: &IssueRow) -> bool {
        self.seen_updated_at
            .is_some_and(|seen| seen.timestamp() != row.updated_at.timestamp())
            || self.seen_comments.is_some_and(|seen| seen != row.comments)
    }
}

/// An indexed issue plus whether its raw JSON says it is a pull request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexedIssue {
    pub row: IssueRow,
    /// GitHub's issues API also lists PRs (`pull_request` key); they are never candidates.
    pub is_pull_request: bool,
}

const REPO_COLUMNS: &str = "repo, archived, fork, license, default_branch, language, description, \
     pushed_at, stars, topics_json, etag, fetched_at";

type RepoRow = (
    String,
    bool,
    bool,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<i64>,
    i64,
    String,
    Option<String>,
    i64,
);

fn repo_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RepoRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
        row.get(11)?,
    ))
}

fn into_profile(row: RepoRow) -> Result<(Cached<RepoProfile>, Option<String>)> {
    let (
        repo,
        archived,
        fork,
        license,
        default_branch,
        language,
        description,
        pushed_at,
        stars,
        topics,
        etag,
        fetched_at,
    ) = row;
    let fetched_at = from_unix(fetched_at)?;
    let profile = RepoProfile {
        repo: repo
            .parse()
            .with_context(|| format!("stored repo {repo:?}"))?,
        description,
        archived,
        fork,
        license,
        default_branch,
        language,
        topics: serde_json::from_str(&topics)
            .with_context(|| format!("stored topics of {repo}"))?,
        stars: u64::try_from(stars).unwrap_or_default(),
        pushed_at: pushed_at.map(from_unix).transpose()?,
        fetched_at,
        activity: None,
    };
    Ok((
        Cached {
            data: profile,
            fetched_at,
        },
        etag,
    ))
}

fn opt_time(value: Option<i64>) -> Result<Option<DateTime<Utc>>> {
    value.map(from_unix).transpose()
}

impl Store {
    /// Cached profile (without activity) and its ETag.
    pub fn repo(&self, repo: &RepoName) -> Result<Option<(Cached<RepoProfile>, Option<String>)>> {
        let sql = format!("SELECT {REPO_COLUMNS} FROM repos WHERE repo = ?1");
        let row = self
            .conn
            .query_row(&sql, params![repo.to_string()], repo_row)
            .optional()?;
        row.map(into_profile).transpose()
    }

    /// Cached profiles for many repositories (missing ones are skipped).
    pub fn repos(&self, repos: &[RepoName]) -> Result<Vec<RepoProfile>> {
        let mut out = Vec::new();
        for repo in repos {
            if let Some((cached, _)) = self.repo(repo)? {
                out.push(cached.data);
            }
        }
        Ok(out)
    }

    /// Upserts metadata. `raw` is GitHub's JSON, kept for later fields.
    pub fn save_repo(
        &mut self,
        profile: &RepoProfile,
        raw: &Value,
        etag: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO repos (repo, owner, archived, fork, license, default_branch, language,
                 description, pushed_at, stars, topics_json, raw_json, etag, fetched_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
             ON CONFLICT(repo) DO UPDATE SET owner = excluded.owner,
                 archived = excluded.archived, fork = excluded.fork,
                 license = excluded.license, default_branch = excluded.default_branch,
                 language = excluded.language, description = excluded.description,
                 pushed_at = excluded.pushed_at, stars = excluded.stars,
                 topics_json = excluded.topics_json, raw_json = excluded.raw_json,
                 etag = excluded.etag, fetched_at = excluded.fetched_at",
            params![
                profile.repo.to_string(),
                profile.repo.owner(),
                profile.archived,
                profile.fork,
                profile.license,
                profile.default_branch,
                profile.language,
                profile.description,
                profile.pushed_at.map(|t| t.timestamp()),
                i64::try_from(profile.stars).unwrap_or(i64::MAX),
                serde_json::to_string(&profile.topics)?,
                serde_json::to_string(raw)?,
                etag,
                profile.fetched_at.timestamp(),
            ],
        )?;
        Ok(())
    }

    /// Marks a cached copy as revalidated by a 304 at `at`.
    pub fn touch_repo(&mut self, repo: &RepoName, at: DateTime<Utc>) -> Result<()> {
        self.conn.execute(
            "UPDATE repos SET fetched_at = MAX(fetched_at, ?2) WHERE repo = ?1",
            params![repo.to_string(), at.timestamp()],
        )?;
        Ok(())
    }

    pub fn activity(&self, repo: &RepoName) -> Result<Option<RepoActivity>> {
        let row = self
            .conn
            .query_row(
                "SELECT external_pr_merge_rate, median_first_response_secs, maintainer_last_active,
                        sample_size, computed_at
                 FROM repo_activity WHERE repo = ?1",
                params![repo.to_string()],
                |row| {
                    Ok((
                        row.get::<_, Option<f64>>(0)?,
                        row.get::<_, Option<i64>>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, i64>(4)?,
                    ))
                },
            )
            .optional()?;
        let Some((rate, response, last_active, sample, computed_at)) = row else {
            return Ok(None);
        };
        Ok(Some(RepoActivity {
            external_pr_merge_rate: rate,
            median_first_response_secs: response.and_then(|s| u64::try_from(s).ok()),
            maintainer_last_active: opt_time(last_active)?,
            sample_size: u32::try_from(sample).unwrap_or_default(),
            computed_at: from_unix(computed_at)?,
        }))
    }

    /// Upserts activity metrics; `raw` is the GraphQL `data` they were computed from.
    pub fn save_activity(
        &mut self,
        repo: &RepoName,
        activity: &RepoActivity,
        raw: Option<&Value>,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO repo_activity (repo, external_pr_merge_rate, median_first_response_secs,
                 maintainer_last_active, sample_size, raw_json, computed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(repo) DO UPDATE SET
                 external_pr_merge_rate = excluded.external_pr_merge_rate,
                 median_first_response_secs = excluded.median_first_response_secs,
                 maintainer_last_active = excluded.maintainer_last_active,
                 sample_size = excluded.sample_size, raw_json = excluded.raw_json,
                 computed_at = excluded.computed_at",
            params![
                repo.to_string(),
                activity.external_pr_merge_rate,
                activity
                    .median_first_response_secs
                    .map(|s| i64::try_from(s).unwrap_or(i64::MAX)),
                activity.maintainer_last_active.map(|t| t.timestamp()),
                activity.sample_size,
                raw.map(serde_json::to_string).transpose()?,
                activity.computed_at.timestamp(),
            ],
        )?;
        Ok(())
    }

    /// Open, unassigned issues from the local index (zero requests), newest
    /// update first. `repos` empty means every indexed repository.
    pub fn open_unassigned_issues(&self, repos: &[RepoName]) -> Result<Vec<IndexedIssue>> {
        let mut statement = self.conn.prepare(
            "SELECT repo, number, title, state, labels_json, assignees_json, comments,
                    created_at, updated_at, fetched_at, raw_json
             FROM issues
             WHERE state = 'open' AND (?1 IS NULL OR repo = ?1)
             ORDER BY updated_at DESC, repo, number",
        )?;
        let single = match repos {
            [repo] => Some(repo.to_string()),
            _ => None,
        };
        let rows = statement
            .query_map(params![single], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, Option<i64>>(7)?,
                    row.get::<_, i64>(8)?,
                    row.get::<_, i64>(9)?,
                    row.get::<_, Option<String>>(10)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let wanted: Vec<String> = repos.iter().map(RepoName::key).collect();
        let mut out = Vec::new();
        for (
            repo,
            number,
            title,
            state,
            labels,
            assignees,
            comments,
            created,
            updated,
            fetched,
            raw,
        ) in rows
        {
            let repo: RepoName = repo
                .parse()
                .with_context(|| format!("stored issue repo {repo:?}"))?;
            if !wanted.is_empty() && !wanted.contains(&repo.key()) {
                continue;
            }
            let assignees: Vec<String> = serde_json::from_str(&assignees).unwrap_or_default();
            if !assignees.is_empty() {
                continue;
            }
            let is_pull_request = raw
                .as_deref()
                .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                .is_some_and(|raw| raw.get("pull_request").is_some_and(|v| !v.is_null()));
            out.push(IndexedIssue {
                row: IssueRow {
                    issue: IssueRef::new(repo, u64::try_from(number).unwrap_or_default()),
                    title,
                    state: IssueState::from_stored(&state),
                    labels: labels_from_json(&labels),
                    assignees,
                    comments: u32::try_from(comments).unwrap_or_default(),
                    created_at: opt_time(created)?,
                    updated_at: from_unix(updated)?,
                    fetched_at: from_unix(fetched)?,
                },
                is_pull_request,
            });
        }
        Ok(out)
    }

    /// Issue marks that are active at `now` (`expires_at` NULL or later).
    pub fn active_issue_marks(&self, now: DateTime<Utc>) -> Result<Vec<StoredIssueMark>> {
        let mut statement = self.conn.prepare(
            "SELECT repo, number, verdict, reason, note, seen_updated_at, seen_comments,
                    marked_by, marked_at, expires_at
             FROM issue_marks
             WHERE expires_at IS NULL OR expires_at > ?1
             ORDER BY marked_at DESC",
        )?;
        let rows = statement
            .query_map(params![now.timestamp()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<i64>>(5)?,
                    row.get::<_, Option<i64>>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, i64>(8)?,
                    row.get::<_, Option<i64>>(9)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(
                |(repo, number, verdict, reason, note, seen_at, seen_comments, by, at, expires)| {
                    let repo: RepoName = repo
                        .parse()
                        .with_context(|| format!("stored issue mark repo {repo:?}"))?;
                    Ok(StoredIssueMark {
                        issue: IssueRef::new(repo, u64::try_from(number).unwrap_or_default()),
                        verdict,
                        reason,
                        note,
                        seen_updated_at: opt_time(seen_at)?,
                        seen_comments: seen_comments.and_then(|c| u32::try_from(c).ok()),
                        marked_by: by,
                        marked_at: from_unix(at)?,
                        expires_at: opt_time(expires)?,
                    })
                },
            )
            .collect()
    }
}

/// Labels are stored as a JSON array of names (or of GitHub label objects).
fn labels_from_json(text: &str) -> Vec<String> {
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Array(items)) => items
            .into_iter()
            .filter_map(|item| match item {
                Value::String(name) => Some(name),
                Value::Object(map) => map.get("name").and_then(Value::as_str).map(str::to_owned),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}
