//! `issues`, `sync_state` (§6, §7). Step 5 owns this file.

use super::{from_unix, Cached, Store};
use crate::error::Result;
use crate::models::{IssueRef, IssueRow, IssueState, RepoName};
use anyhow::Context;
use chrono::{DateTime, Utc};
use rusqlite::types::Value as SqlValue;
use rusqlite::{params, params_from_iter, OptionalExtension, Row, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Incremental sync watermark for one repository.
///
/// Invariant: every issue (and PR) of the repository whose `updated_at` is at
/// or before `since` has been stored, as far as the listing order guarantees
/// (see `index.rs`). The next pass requests `since - index_overlap`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SyncState {
    /// `since` for the next `sort=updated` pass (before subtracting the overlap).
    pub since: Option<DateTime<Utc>>,
    /// When the first full listing (open issues) completed; `None` until then.
    pub last_full_at: Option<DateTime<Utc>>,
    /// When the last pass (complete or not) started.
    pub last_sync_at: Option<DateTime<Utc>>,
    /// The last pass stopped early (budget, quota, network); the next run resumes
    /// from `since` and ignores the TTL.
    pub partial: bool,
    /// Start of the earliest run of an unfinished pass; becomes the watermark
    /// when the pass completes.
    pub pass_started_at: Option<DateTime<Utc>>,
}

/// Extra state kept in `sync_state.state_json`.
#[derive(Default, Serialize, Deserialize)]
struct StateJson {
    #[serde(default)]
    partial: bool,
    #[serde(default)]
    pass_started_at: Option<i64>,
}

/// Local filter for `candidates` (zero requests).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IssueFilter {
    pub repos: Vec<RepoName>,
    /// Repository language from the `repos` table (case-insensitive).
    pub language: Option<String>,
    /// Keep issues carrying at least one of these labels (case-insensitive).
    pub labels: Vec<String>,
    pub open_only: bool,
    pub unassigned_only: bool,
    pub limit: Option<u32>,
}

/// What [`Store::save_issues`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SaveStats {
    /// Rows that did not exist before.
    pub new: u32,
    /// Existing rows whose index fields changed.
    pub changed: u32,
    /// Rows already up to date, or older than the stored copy (not written).
    pub unchanged: u32,
}

const ISSUE_COLUMNS: &str = "repo, number, title, state, labels_json, assignees_json, comments, \
     created_at, updated_at, fetched_at";

struct RawIssue {
    repo: String,
    number: i64,
    title: String,
    state: String,
    labels: String,
    assignees: String,
    comments: i64,
    created_at: Option<i64>,
    updated_at: i64,
    fetched_at: i64,
}

fn raw_issue(row: &Row<'_>) -> rusqlite::Result<RawIssue> {
    Ok(RawIssue {
        repo: row.get(0)?,
        number: row.get(1)?,
        title: row.get(2)?,
        state: row.get(3)?,
        labels: row.get(4)?,
        assignees: row.get(5)?,
        comments: row.get(6)?,
        created_at: row.get(7)?,
        updated_at: row.get(8)?,
        fetched_at: row.get(9)?,
    })
}

impl RawIssue {
    fn into_row(self) -> Result<IssueRow> {
        let reference = format!("{}#{}", self.repo, self.number);
        let context = |what: &str| format!("stored issue {reference} {what}");
        Ok(IssueRow {
            issue: IssueRef::new(
                self.repo.parse().with_context(|| context("repo"))?,
                u64::try_from(self.number).with_context(|| context("number"))?,
            ),
            title: self.title,
            state: IssueState::from_stored(&self.state),
            labels: serde_json::from_str(&self.labels).with_context(|| context("labels"))?,
            assignees: serde_json::from_str(&self.assignees)
                .with_context(|| context("assignees"))?,
            comments: u32::try_from(self.comments).unwrap_or(0),
            created_at: self.created_at.map(from_unix).transpose()?,
            updated_at: from_unix(self.updated_at)?,
            fetched_at: from_unix(self.fetched_at)?,
        })
    }
}

fn same_index_fields(a: &IssueRow, b: &IssueRow) -> bool {
    a.title == b.title
        && a.state == b.state
        && a.labels == b.labels
        && a.assignees == b.assignees
        && a.comments == b.comments
        && a.updated_at == b.updated_at
}

impl Store {
    pub fn issue(&self, issue: &IssueRef) -> Result<Option<Cached<IssueRow>>> {
        let sql = format!("SELECT {ISSUE_COLUMNS} FROM issues WHERE repo = ?1 AND number = ?2");
        let raw = self
            .conn
            .query_row(
                &sql,
                params![issue.repo.to_string(), i64::try_from(issue.number)?],
                raw_issue,
            )
            .optional()?;
        raw.map(|raw| {
            let row = raw.into_row()?;
            Ok(Cached {
                fetched_at: row.fetched_at,
                data: row,
            })
        })
        .transpose()
    }

    /// Upserts index fields; `raw` is GitHub's JSON for the issue. A row older
    /// than the stored copy (by `updated_at`) is ignored, so an overlapping
    /// sync window or a stale detail read never regresses the index.
    pub fn save_issues(&mut self, rows: &[(IssueRow, Value)]) -> Result<SaveStats> {
        let mut stats = SaveStats::default();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        {
            let select =
                format!("SELECT {ISSUE_COLUMNS} FROM issues WHERE repo = ?1 AND number = ?2");
            let mut select = tx.prepare(&select)?;
            let mut upsert = tx.prepare(
                "INSERT INTO issues (repo, number, title, state, labels_json, assignees_json,
                     comments, created_at, updated_at, raw_json, fetched_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                 ON CONFLICT(repo, number) DO UPDATE SET title = excluded.title,
                     state = excluded.state, labels_json = excluded.labels_json,
                     assignees_json = excluded.assignees_json, comments = excluded.comments,
                     created_at = COALESCE(excluded.created_at, issues.created_at),
                     updated_at = excluded.updated_at, raw_json = excluded.raw_json,
                     fetched_at = excluded.fetched_at",
            )?;
            for (row, raw) in rows {
                let repo = row.issue.repo.to_string();
                let number = i64::try_from(row.issue.number)?;
                let existing = select
                    .query_row(params![repo, number], raw_issue)
                    .optional()?
                    .map(RawIssue::into_row)
                    .transpose()?;
                match &existing {
                    Some(old) if row.updated_at < old.updated_at => {
                        stats.unchanged += 1;
                        continue;
                    }
                    Some(old) if same_index_fields(old, row) => stats.unchanged += 1,
                    Some(_) => stats.changed += 1,
                    None => stats.new += 1,
                }
                upsert.execute(params![
                    repo,
                    number,
                    row.title,
                    row.state.as_str(),
                    serde_json::to_string(&row.labels)?,
                    serde_json::to_string(&row.assignees)?,
                    i64::from(row.comments),
                    row.created_at.map(|t| t.timestamp()),
                    row.updated_at.timestamp(),
                    raw.to_string(),
                    row.fetched_at.timestamp(),
                ])?;
            }
        }
        tx.commit()?;
        Ok(stats)
    }

    /// Indexed issues matching `filter`, most recently updated first.
    pub fn query_issues(&self, filter: &IssueFilter) -> Result<Vec<IssueRow>> {
        let mut sql = format!("SELECT {ISSUE_COLUMNS} FROM issues WHERE 1 = 1");
        let mut values: Vec<SqlValue> = Vec::new();
        if !filter.repos.is_empty() {
            let marks = vec!["?"; filter.repos.len()].join(", ");
            sql.push_str(&format!(" AND repo IN ({marks})"));
            values.extend(filter.repos.iter().map(|r| SqlValue::Text(r.to_string())));
        }
        if let Some(language) = &filter.language {
            sql.push_str(" AND repo IN (SELECT repo FROM repos WHERE language = ?)");
            values.push(SqlValue::Text(language.clone()));
        }
        if filter.open_only {
            sql.push_str(" AND state = 'open'");
        }
        if filter.unassigned_only {
            sql.push_str(" AND assignees_json = '[]'");
        }
        sql.push_str(" ORDER BY updated_at DESC, repo, number");
        let mut stmt = self.conn.prepare(&sql)?;
        let raws = stmt
            .query_map(params_from_iter(values), raw_issue)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let wanted: Vec<String> = filter
            .labels
            .iter()
            .map(|l| l.to_ascii_lowercase())
            .collect();
        let mut rows = Vec::new();
        for raw in raws {
            let row = raw.into_row()?;
            if !wanted.is_empty()
                && !row
                    .labels
                    .iter()
                    .any(|label| wanted.contains(&label.to_ascii_lowercase()))
            {
                continue;
            }
            rows.push(row);
            if filter
                .limit
                .is_some_and(|limit| rows.len() >= limit as usize)
            {
                break;
            }
        }
        Ok(rows)
    }

    pub fn sync_state(&self, repo: &RepoName) -> Result<SyncState> {
        let row = self
            .conn
            .query_row(
                "SELECT since, last_full_at, last_sync_at, state_json FROM sync_state WHERE repo = ?1",
                params![repo.to_string()],
                |row| {
                    Ok((
                        row.get::<_, Option<i64>>(0)?,
                        row.get::<_, Option<i64>>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                },
            )
            .optional()?;
        let Some((since, last_full_at, last_sync_at, json)) = row else {
            return Ok(SyncState::default());
        };
        let extra: StateJson = match json {
            Some(json) => serde_json::from_str(&json)
                .with_context(|| format!("stored sync state for {repo}"))?,
            None => StateJson::default(),
        };
        Ok(SyncState {
            since: since.map(from_unix).transpose()?,
            last_full_at: last_full_at.map(from_unix).transpose()?,
            last_sync_at: last_sync_at.map(from_unix).transpose()?,
            partial: extra.partial,
            pass_started_at: extra.pass_started_at.map(from_unix).transpose()?,
        })
    }

    pub fn save_sync_state(&mut self, repo: &RepoName, state: &SyncState) -> Result<()> {
        let extra = StateJson {
            partial: state.partial,
            pass_started_at: state.pass_started_at.map(|t| t.timestamp()),
        };
        self.conn.execute(
            "INSERT INTO sync_state (repo, since, last_full_at, last_sync_at, state_json)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(repo) DO UPDATE SET since = excluded.since,
                 last_full_at = excluded.last_full_at, last_sync_at = excluded.last_sync_at,
                 state_json = excluded.state_json",
            params![
                repo.to_string(),
                state.since.map(|t| t.timestamp()),
                state.last_full_at.map(|t| t.timestamp()),
                state.last_sync_at.map(|t| t.timestamp()),
                serde_json::to_string(&extra)?,
            ],
        )?;
        Ok(())
    }
}
