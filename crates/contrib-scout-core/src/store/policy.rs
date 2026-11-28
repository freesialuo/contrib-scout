//! `policy_marks`, `policy_mark_history`, `policy_files` (§5, §6).
//!
//! Mark storage, plus the policy-file cache (raw candidate files, snapshot
//! markers, and cached absence).

use super::{from_unix, Cached, Store};
use crate::error::Result;
use crate::models::{
    AgentId, MarkAction, MarkTarget, PolicyFile, PolicyMark, PolicyMarkEvent, RepoName, Stance,
};
use anyhow::Context;
use chrono::{DateTime, Utc};
use rusqlite::{params, OptionalExtension, Row, TransactionBehavior};

/// One `policy_files` row. Three kinds share the table, all keyed by
/// `(repo, path)`:
/// - the per-repository snapshot marker ([`POLICY_SNAPSHOT_PATH`]): `status`
///   is the `commits/HEAD` answer (200, or 404 / 409 when the repository is
///   missing or empty), `commit` the HEAD commit, `etag` its validator;
/// - a found candidate file: `status` 200 and `file` set;
/// - an absent candidate path: `status` 404, `file` `None`, `url` the
///   commit-pinned location that was checked. Absence is cached too.
#[derive(Clone, Debug, PartialEq)]
pub struct CachedPolicyFile {
    pub repo: RepoName,
    pub path: String,
    pub status: u16,
    pub file: Option<PolicyFile>,
    /// Commit the probe was made at (marker and absent rows).
    pub commit: Option<String>,
    /// Pinned URL: the file's for found rows, the checked location for absent rows.
    pub url: Option<String>,
    pub etag: Option<String>,
    pub fetched_at: DateTime<Utc>,
}

/// `path` of the per-repository snapshot marker row in `policy_files`.
pub const POLICY_SNAPSHOT_PATH: &str = "";

const POLICY_FILE_COLUMNS: &str =
    "repo, path, status, url, sha, text, size, truncated, etag, fetched_at";

struct PolicyFileRow {
    repo: String,
    path: String,
    status: u16,
    url: Option<String>,
    sha: Option<String>,
    text: Option<String>,
    size: i64,
    truncated: bool,
    etag: Option<String>,
    fetched_at: i64,
}

fn policy_file_from_row(row: &Row<'_>) -> rusqlite::Result<PolicyFileRow> {
    Ok(PolicyFileRow {
        repo: row.get(0)?,
        path: row.get(1)?,
        status: row.get(2)?,
        url: row.get(3)?,
        sha: row.get(4)?,
        text: row.get(5)?,
        size: row.get(6)?,
        truncated: row.get(7)?,
        etag: row.get(8)?,
        fetched_at: row.get(9)?,
    })
}

impl PolicyFileRow {
    fn into_cached(self) -> Result<Cached<CachedPolicyFile>> {
        let repo: RepoName = self
            .repo
            .parse()
            .with_context(|| format!("stored policy file repo {:?}", self.repo))?;
        let fetched_at = from_unix(self.fetched_at)?;
        let is_file = self.status == 200 && self.path != POLICY_SNAPSHOT_PATH;
        let file = is_file.then(|| PolicyFile {
            repo: repo.clone(),
            path: self.path.clone(),
            url: self.url.clone().unwrap_or_default(),
            text: self.text.clone(),
            truncated: self.truncated,
            size: u64::try_from(self.size).unwrap_or_default(),
            sha: self.sha.clone(),
            fetched_at,
        });
        Ok(Cached {
            data: CachedPolicyFile {
                repo,
                path: self.path,
                status: self.status,
                file,
                commit: if is_file { None } else { self.sha },
                url: self.url,
                etag: self.etag,
                fetched_at,
            },
            fetched_at,
        })
    }
}

fn insert_policy_file(conn: &rusqlite::Connection, file: &CachedPolicyFile) -> Result<()> {
    let (url, sha, text, size, truncated) = match &file.file {
        Some(found) => (
            Some(found.url.clone()),
            found.sha.clone(),
            found.text.clone(),
            i64::try_from(found.size).unwrap_or(i64::MAX),
            found.truncated,
        ),
        None => (file.url.clone(), file.commit.clone(), None, 0, false),
    };
    conn.execute(
        "INSERT INTO policy_files (repo, path, status, url, sha, text, size, truncated, etag, fetched_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
         ON CONFLICT(repo, path) DO UPDATE SET status = excluded.status, url = excluded.url,
             sha = excluded.sha, text = excluded.text, size = excluded.size,
             truncated = excluded.truncated, etag = excluded.etag,
             fetched_at = excluded.fetched_at",
        params![
            file.repo.to_string(),
            file.path,
            file.status,
            url,
            sha,
            text,
            size,
            truncated,
            file.etag,
            file.fetched_at.timestamp(),
        ],
    )?;
    Ok(())
}

const MARK_COLUMNS: &str = "target, target_kind, stance, scope_json, requirements_json, \
     evidence_json, note, marked_by, marked_at, expires_at";

/// Raw columns; converted outside rusqlite so parse errors keep their context.
struct MarkRow {
    target: String,
    kind: String,
    stance: String,
    scope: String,
    requirements: String,
    evidence: String,
    note: String,
    marked_by: String,
    marked_at: i64,
    expires_at: i64,
}

fn mark_from_row(row: &Row<'_>) -> rusqlite::Result<MarkRow> {
    Ok(MarkRow {
        target: row.get(0)?,
        kind: row.get(1)?,
        stance: row.get(2)?,
        scope: row.get(3)?,
        requirements: row.get(4)?,
        evidence: row.get(5)?,
        note: row.get(6)?,
        marked_by: row.get(7)?,
        marked_at: row.get(8)?,
        expires_at: row.get(9)?,
    })
}

fn parse_target(key: &str, kind: &str) -> Result<MarkTarget> {
    Ok(match kind {
        "org" => MarkTarget::Org(key.parse().with_context(|| format!("stored org {key:?}"))?),
        _ => MarkTarget::Repo(
            key.parse()
                .with_context(|| format!("stored repo {key:?}"))?,
        ),
    })
}

impl MarkRow {
    fn into_mark(self) -> Result<PolicyMark> {
        let target = self.target.clone();
        let context = |what: &str| format!("stored policy mark {target} {what}");
        Ok(PolicyMark {
            target: parse_target(&self.target, &self.kind)?,
            stance: Stance::from_stored(&self.stance),
            scope: serde_json::from_str(&self.scope).with_context(|| context("scope"))?,
            requirements: serde_json::from_str(&self.requirements)
                .with_context(|| context("requirements"))?,
            evidence: serde_json::from_str(&self.evidence).with_context(|| context("evidence"))?,
            note: self.note,
            marked_by: self
                .marked_by
                .parse()
                .with_context(|| context("marked_by"))?,
            marked_at: from_unix(self.marked_at)?,
            expires_at: from_unix(self.expires_at)?,
        })
    }
}

fn history_row(
    tx: &rusqlite::Transaction<'_>,
    target: &MarkTarget,
    action: &MarkAction,
    mark: Option<&PolicyMark>,
    agent: &AgentId,
    at: DateTime<Utc>,
) -> Result<()> {
    let mark_json = mark.map(serde_json::to_string).transpose()?;
    tx.execute(
        "INSERT INTO policy_mark_history (target, target_kind, action, mark_json, agent_id, at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            target.key(),
            target.kind(),
            action.as_str(),
            mark_json,
            agent.as_str(),
            at.timestamp()
        ],
    )?;
    Ok(())
}

impl Store {
    /// The mark stored for exactly this target.
    pub fn mark(&self, target: &MarkTarget) -> Result<Option<PolicyMark>> {
        let sql = format!("SELECT {MARK_COLUMNS} FROM policy_marks WHERE target = ?1");
        self.conn
            .query_row(&sql, params![target.key()], mark_from_row)
            .optional()?
            .map(MarkRow::into_mark)
            .transpose()
    }

    /// The mark that applies to `repo`: its own, else its org's (§5 step 1).
    pub fn effective_mark(&self, repo: &RepoName) -> Result<Option<PolicyMark>> {
        if let Some(mark) = self.mark(&MarkTarget::Repo(repo.clone()))? {
            return Ok(Some(mark));
        }
        let org = MarkTarget::Org(repo.owner().parse()?);
        self.mark(&org)
    }

    /// Upserts the mark and appends a `set` history row in one transaction.
    /// Callers validate first ([`PolicyMark::validate`]).
    pub fn put_mark(&mut self, mark: &PolicyMark) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO policy_marks (target, target_kind, stance, scope_json, requirements_json,
                 evidence_json, note, marked_by, marked_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(target) DO UPDATE SET target_kind = excluded.target_kind,
                 stance = excluded.stance, scope_json = excluded.scope_json,
                 requirements_json = excluded.requirements_json,
                 evidence_json = excluded.evidence_json, note = excluded.note,
                 marked_by = excluded.marked_by, marked_at = excluded.marked_at,
                 expires_at = excluded.expires_at",
            params![
                mark.target.key(),
                mark.target.kind(),
                mark.stance.as_str(),
                serde_json::to_string(&mark.scope)?,
                serde_json::to_string(&mark.requirements)?,
                serde_json::to_string(&mark.evidence)?,
                mark.note,
                mark.marked_by.as_str(),
                mark.marked_at.timestamp(),
                mark.expires_at.timestamp(),
            ],
        )?;
        history_row(
            &tx,
            &mark.target,
            &MarkAction::Set,
            Some(mark),
            &mark.marked_by,
            mark.marked_at,
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Deletes the mark and, if one existed, appends an `unset` history row
    /// holding it. Returns the removed mark.
    pub fn delete_mark(
        &mut self,
        target: &MarkTarget,
        agent: &AgentId,
        at: DateTime<Utc>,
    ) -> Result<Option<PolicyMark>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        // Read under the write lock, so the history row holds what is deleted.
        let sql = format!("SELECT {MARK_COLUMNS} FROM policy_marks WHERE target = ?1");
        let existing = tx
            .query_row(&sql, params![target.key()], mark_from_row)
            .optional()?
            .map(MarkRow::into_mark)
            .transpose()?;
        if existing.is_none() {
            return Ok(None);
        }
        tx.execute(
            "DELETE FROM policy_marks WHERE target = ?1",
            params![target.key()],
        )?;
        history_row(
            &tx,
            target,
            &MarkAction::Unset,
            existing.as_ref(),
            agent,
            at,
        )?;
        tx.commit()?;
        Ok(existing)
    }

    /// History for a target, oldest first.
    pub fn mark_history(&self, target: &MarkTarget) -> Result<Vec<PolicyMarkEvent>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, target, target_kind, action, mark_json, agent_id, at
             FROM policy_mark_history WHERE target = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map(params![target.key()], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, i64>(6)?,
            ))
        })?;
        let mut events = Vec::new();
        for row in rows {
            let (id, key, kind, action, mark_json, agent, at) = row?;
            events.push(PolicyMarkEvent {
                id,
                target: parse_target(&key, &kind)?,
                action: MarkAction::from_stored(&action),
                mark: mark_json
                    .map(|json| serde_json::from_str(&json))
                    .transpose()
                    .with_context(|| format!("stored mark history {id}"))?,
                agent: agent
                    .parse()
                    .with_context(|| format!("stored mark history {id} agent"))?,
                at: from_unix(at)?,
            });
        }
        Ok(events)
    }

    /// Marks with `expires_at <= now` (`policy due`). Expired marks are never deleted.
    pub fn due_marks(&self, now: DateTime<Utc>) -> Result<Vec<PolicyMark>> {
        let sql = format!(
            "SELECT {MARK_COLUMNS} FROM policy_marks WHERE expires_at <= ?1 ORDER BY expires_at"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![now.timestamp()], mark_from_row)?;
        rows.map(|row| row?.into_mark()).collect()
    }

    /// Every stored mark, repo and org, ordered by target (case-insensitively,
    /// as the key is unique; `policy list`). Never deletes or rewrites.
    pub fn marks(&self) -> Result<Vec<PolicyMark>> {
        let sql = format!("SELECT {MARK_COLUMNS} FROM policy_marks ORDER BY target");
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([], mark_from_row)?;
        rows.map(|row| row?.into_mark()).collect()
    }

    /// Effective marks for many repos at once (discovery's zero-request policy step, §7).
    /// Repos without a mark (own or org) are omitted.
    pub fn effective_marks(&self, repos: &[RepoName]) -> Result<Vec<(RepoName, PolicyMark)>> {
        let mut marks = Vec::new();
        for repo in repos {
            if let Some(mark) = self.effective_mark(repo)? {
                marks.push((repo.clone(), mark));
            }
        }
        Ok(marks)
    }

    /// One `policy_files` row. `path` matches exactly first, then
    /// case-insensitively (GitHub paths are case-sensitive, agents' typing is not).
    pub fn cached_policy_file(
        &self,
        repo: &RepoName,
        path: &str,
    ) -> Result<Option<Cached<CachedPolicyFile>>> {
        let exact =
            format!("SELECT {POLICY_FILE_COLUMNS} FROM policy_files WHERE repo = ?1 AND path = ?2");
        let row = match self
            .conn
            .query_row(
                &exact,
                params![repo.to_string(), path],
                policy_file_from_row,
            )
            .optional()?
        {
            Some(row) => Some(row),
            None => {
                let loose = format!(
                    "SELECT {POLICY_FILE_COLUMNS} FROM policy_files
                     WHERE repo = ?1 AND path = ?2 COLLATE NOCASE ORDER BY status, path LIMIT 1"
                );
                self.conn
                    .query_row(
                        &loose,
                        params![repo.to_string(), path],
                        policy_file_from_row,
                    )
                    .optional()?
            }
        };
        row.map(PolicyFileRow::into_cached).transpose()
    }

    /// The cached row whose pinned URL is `url`, if any (evidence given as a URL).
    pub fn cached_policy_file_by_url(&self, url: &str) -> Result<Option<Cached<CachedPolicyFile>>> {
        let sql = format!(
            "SELECT {POLICY_FILE_COLUMNS} FROM policy_files WHERE url = ?1 ORDER BY fetched_at DESC LIMIT 1"
        );
        self.conn
            .query_row(&sql, params![url], policy_file_from_row)
            .optional()?
            .map(PolicyFileRow::into_cached)
            .transpose()
    }

    /// Every row cached for `repo` (marker included), ordered by path.
    pub fn cached_policy_files(&self, repo: &RepoName) -> Result<Vec<Cached<CachedPolicyFile>>> {
        let sql =
            format!("SELECT {POLICY_FILE_COLUMNS} FROM policy_files WHERE repo = ?1 ORDER BY path");
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![repo.to_string()], policy_file_from_row)?;
        rows.map(|row| row?.into_cached()).collect()
    }

    /// Upserts one row.
    pub fn save_policy_file(&mut self, file: &CachedPolicyFile) -> Result<()> {
        insert_policy_file(&self.conn, file)
    }

    /// Replaces every row of `repo` with `files` in one transaction (a new
    /// snapshot: renamed or deleted files must not linger).
    pub fn replace_policy_files(
        &mut self,
        repo: &RepoName,
        files: &[CachedPolicyFile],
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "DELETE FROM policy_files WHERE repo = ?1",
            params![repo.to_string()],
        )?;
        for file in files {
            insert_policy_file(&tx, file)?;
        }
        tx.commit()?;
        Ok(())
    }
}
