//! `issue_marks`, `issue_mark_history` (§5b, §6). Step 5 owns this file.
//!
//! Column semantics are shared with `candidates`, which reads `issue_marks`
//! directly: `issue_ref` `owner/repo#N`, `verdict` `skip` | `shortlist`,
//! `expires_at` NULL = never, `seen_updated_at` / `seen_comments` = snapshot.

use super::{from_unix, Store};
use crate::error::Result;
use crate::issue_marks::{
    IssueMark, IssueMarkAction, IssueMarkEvent, IssueMarkFilter, IssueMarkStatus, IssueVerdict,
    SkipReason,
};
use crate::models::{AgentId, IssueRef};
use anyhow::{anyhow, Context};
use chrono::{DateTime, Utc};
use rusqlite::types::Value as SqlValue;
use rusqlite::{
    params, params_from_iter, OptionalExtension, Row, Transaction, TransactionBehavior,
};

const COLUMNS: &str = "issue_ref, verdict, reason, note, seen_updated_at, seen_comments, \
     marked_by, marked_at, expires_at";

struct RawMark {
    issue_ref: String,
    verdict: String,
    reason: Option<String>,
    note: String,
    seen_updated_at: Option<i64>,
    seen_comments: Option<i64>,
    marked_by: String,
    marked_at: i64,
    expires_at: Option<i64>,
}

fn raw_mark(row: &Row<'_>) -> rusqlite::Result<RawMark> {
    Ok(RawMark {
        issue_ref: row.get(0)?,
        verdict: row.get(1)?,
        reason: row.get(2)?,
        note: row.get(3)?,
        seen_updated_at: row.get(4)?,
        seen_comments: row.get(5)?,
        marked_by: row.get(6)?,
        marked_at: row.get(7)?,
        expires_at: row.get(8)?,
    })
}

impl RawMark {
    fn into_mark(self) -> Result<IssueMark> {
        let reference = self.issue_ref.clone();
        let context = |what: &str| format!("stored issue mark {reference} {what}");
        Ok(IssueMark {
            issue: self.issue_ref.parse().with_context(|| context("ref"))?,
            verdict: match self.verdict.as_str() {
                "skip" => IssueVerdict::Skip,
                "shortlist" => IssueVerdict::Shortlist,
                other => return Err(anyhow!("{}: unknown verdict {other:?}", context("verdict"))),
            },
            reason: self.reason.as_deref().map(SkipReason::from_stored),
            note: self.note,
            seen_updated_at: self.seen_updated_at.map(from_unix).transpose()?,
            seen_comments: self
                .seen_comments
                .map(u32::try_from)
                .transpose()
                .with_context(|| context("seen_comments"))?,
            marked_by: self
                .marked_by
                .parse()
                .with_context(|| context("marked_by"))?,
            marked_at: from_unix(self.marked_at)?,
            expires_at: self.expires_at.map(from_unix).transpose()?,
        })
    }
}

fn history_row(
    tx: &Transaction<'_>,
    issue: &IssueRef,
    action: IssueMarkAction,
    mark: Option<&IssueMark>,
    agent: &AgentId,
    at: DateTime<Utc>,
) -> Result<()> {
    tx.execute(
        "INSERT INTO issue_mark_history (issue_ref, action, mark_json, agent_id, at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            issue.to_string(),
            action.as_str(),
            mark.map(serde_json::to_string).transpose()?,
            agent.as_str(),
            at.timestamp(),
        ],
    )?;
    Ok(())
}

impl Store {
    /// The mark stored for this issue (active or expired).
    pub fn issue_mark(&self, issue: &IssueRef) -> Result<Option<IssueMark>> {
        let sql = format!("SELECT {COLUMNS} FROM issue_marks WHERE issue_ref = ?1");
        self.conn
            .query_row(&sql, params![issue.to_string()], raw_mark)
            .optional()?
            .map(RawMark::into_mark)
            .transpose()
    }

    /// Upserts the mark and appends a `mark` history row in one transaction.
    /// Callers validate first ([`IssueMark::validate`]).
    pub fn put_issue_mark(&mut self, mark: &IssueMark) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO issue_marks (issue_ref, repo, number, verdict, reason, note,
                 seen_updated_at, seen_comments, marked_by, marked_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(issue_ref) DO UPDATE SET repo = excluded.repo,
                 number = excluded.number, verdict = excluded.verdict,
                 reason = excluded.reason, note = excluded.note,
                 seen_updated_at = excluded.seen_updated_at,
                 seen_comments = excluded.seen_comments, marked_by = excluded.marked_by,
                 marked_at = excluded.marked_at, expires_at = excluded.expires_at",
            params![
                mark.issue.to_string(),
                mark.issue.repo.to_string(),
                i64::try_from(mark.issue.number)?,
                mark.verdict.as_str(),
                mark.reason.as_ref().map(SkipReason::as_str),
                mark.note,
                mark.seen_updated_at.map(|t| t.timestamp()),
                mark.seen_comments.map(i64::from),
                mark.marked_by.as_str(),
                mark.marked_at.timestamp(),
                mark.expires_at.map(|t| t.timestamp()),
            ],
        )?;
        history_row(
            &tx,
            &mark.issue,
            IssueMarkAction::Mark,
            Some(mark),
            &mark.marked_by,
            mark.marked_at,
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Deletes the mark and, if one existed, appends an `unmark` history row
    /// holding it. Returns the removed mark.
    pub fn delete_issue_mark(
        &mut self,
        issue: &IssueRef,
        agent: &AgentId,
        at: DateTime<Utc>,
    ) -> Result<Option<IssueMark>> {
        let existing = self.issue_mark(issue)?;
        if existing.is_none() {
            return Ok(None);
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "DELETE FROM issue_marks WHERE issue_ref = ?1",
            params![issue.to_string()],
        )?;
        history_row(
            &tx,
            issue,
            IssueMarkAction::Unmark,
            existing.as_ref(),
            agent,
            at,
        )?;
        tx.commit()?;
        Ok(existing)
    }

    /// History for one issue, oldest first.
    pub fn issue_mark_history(&self, issue: &IssueRef) -> Result<Vec<IssueMarkEvent>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, issue_ref, action, mark_json, agent_id, at
             FROM issue_mark_history WHERE issue_ref = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map(params![issue.to_string()], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)?,
            ))
        })?;
        let mut events = Vec::new();
        for row in rows {
            let (id, reference, action, mark_json, agent, at) = row?;
            let context = || format!("stored issue mark history {id}");
            events.push(IssueMarkEvent {
                id,
                issue: reference.parse().with_context(context)?,
                action: match action.as_str() {
                    "unmark" => IssueMarkAction::Unmark,
                    _ => IssueMarkAction::Mark,
                },
                mark: mark_json
                    .map(|json| serde_json::from_str(&json))
                    .transpose()
                    .with_context(context)?,
                agent: agent.parse().with_context(context)?,
                at: from_unix(at)?,
            });
        }
        Ok(events)
    }

    /// Marks matching `filter`, newest first.
    pub fn issue_marks(&self, filter: &IssueMarkFilter) -> Result<Vec<IssueMark>> {
        let mut sql = format!("SELECT {COLUMNS} FROM issue_marks WHERE 1 = 1");
        let mut values: Vec<SqlValue> = Vec::new();
        if let Some(verdict) = filter.verdict {
            sql.push_str(" AND verdict = ?");
            values.push(SqlValue::Text(verdict.as_str().into()));
        }
        if let Some(repo) = &filter.repo {
            sql.push_str(" AND repo = ?");
            values.push(SqlValue::Text(repo.to_string()));
        }
        if let Some(now) = filter.active_at {
            sql.push_str(" AND (expires_at IS NULL OR expires_at > ?)");
            values.push(SqlValue::Integer(now.timestamp()));
        }
        sql.push_str(" ORDER BY marked_at DESC, issue_ref");
        let mut stmt = self.conn.prepare(&sql)?;
        let raws = stmt
            .query_map(params_from_iter(values), raw_mark)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        raws.into_iter().map(RawMark::into_mark).collect()
    }

    /// The issue's mark compared with its current index row (§5b `changed_since_mark`).
    pub fn issue_mark_status(
        &self,
        issue: &IssueRef,
        now: DateTime<Utc>,
    ) -> Result<Option<IssueMarkStatus>> {
        let Some(mark) = self.issue_mark(issue)? else {
            return Ok(None);
        };
        let row = self.issue(issue)?.map(|cached| cached.data);
        Ok(Some(IssueMarkStatus::new(mark, row.as_ref(), now)))
    }

    /// [`Store::issue_mark_status`] for every mark matching `filter`.
    pub fn issue_mark_statuses(
        &self,
        filter: &IssueMarkFilter,
        now: DateTime<Utc>,
    ) -> Result<Vec<IssueMarkStatus>> {
        self.issue_marks(filter)?
            .into_iter()
            .map(|mark| {
                let row = self.issue(&mark.issue)?.map(|cached| cached.data);
                Ok(IssueMarkStatus::new(mark, row.as_ref(), now))
            })
            .collect()
    }
}
