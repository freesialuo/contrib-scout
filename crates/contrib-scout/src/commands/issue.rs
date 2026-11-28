//! `issue show|mark|unmark|marks` (§5b, §10). Step 5 owns this file.

use super::Context;
use crate::render::Report;
use anyhow::Result;
use chrono::{DateTime, NaiveDate, TimeDelta, Utc};
use clap::Subcommand;
use contrib_scout_core::config::parse_duration;
use contrib_scout_core::index::GitHubIndexer;
use contrib_scout_core::issue_marks::{
    issue_reminders, mark_issue, IssueMarkFilter, IssueMarkStatus, IssueVerdict, NewIssueMark,
    SkipReason,
};
use contrib_scout_core::models::IssueDetail;
use contrib_scout_core::policy::reminder_for;
use contrib_scout_core::store::Store;
use contrib_scout_core::{IssueRef, RepoName, ScoutError};
use serde_json::json;
use std::collections::BTreeSet;
use std::sync::PoisonError;

#[derive(Subcommand, Debug)]
pub enum IssueCommand {
    /// Issue body, recent comments, and linked PRs, with issue-mark and policy reminders
    /// (use --fresh to ignore the TTL).
    Show {
        /// owner/repo#N or an issue URL
        #[arg(value_name = "REF")]
        reference: IssueRef,
    },
    /// Record your verdict on an issue so later agents need not re-read it (local only).
    Mark {
        /// owner/repo#N or an issue URL
        #[arg(value_name = "REF")]
        reference: IssueRef,
        /// skip (do not consider) or shortlist (worth doing)
        #[arg(long)]
        verdict: IssueVerdict,
        /// Required for skip: too_large, unclear, needs_special_env, out_of_scope,
        /// duplicate, maintainer_unresponsive, already_addressed, other
        #[arg(long)]
        reason: Option<SkipReason>,
        /// One sentence shown verbatim in reminders.
        #[arg(long)]
        note: String,
        /// Expiry: a date (YYYY-MM-DD), an RFC 3339 time, or a duration such as 30d.
        /// Default: never expires.
        #[arg(long, value_name = "D")]
        until: Option<String>,
    },
    /// Remove an issue mark.
    Unmark {
        /// owner/repo#N or an issue URL
        #[arg(value_name = "REF")]
        reference: IssueRef,
    },
    /// List issue marks (expired ones included, flagged inactive).
    Marks {
        /// Only this verdict.
        #[arg(long)]
        verdict: Option<IssueVerdict>,
        /// Only this repository.
        #[arg(long, value_name = "OWNER/REPO")]
        repo: Option<RepoName>,
    },
}

pub fn run(ctx: &mut Context<'_>, command: &IssueCommand) -> Result<Report> {
    match command {
        IssueCommand::Show { reference } => show(ctx, reference),
        IssueCommand::Mark {
            reference,
            verdict,
            reason,
            note,
            until,
        } => {
            let expires_at = until
                .as_deref()
                .map(|until| parse_until(until, ctx.now))
                .transpose()?;
            let new = NewIssueMark {
                issue: reference.clone(),
                verdict: *verdict,
                reason: reason.clone(),
                note: note.clone(),
                expires_at,
            };
            mark(ctx, new)
        }
        IssueCommand::Unmark { reference } => unmark(ctx, reference),
        IssueCommand::Marks { verdict, repo } => marks(ctx, *verdict, repo.as_ref()),
    }
}

/// `--until`: `YYYY-MM-DD` (midnight UTC), RFC 3339, or a duration from now.
fn parse_until(value: &str, now: DateTime<Utc>) -> Result<DateTime<Utc>> {
    let value = value.trim();
    if let Ok(date) = NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        return Ok(date
            .and_hms_opt(0, 0, 0)
            .expect("midnight exists")
            .and_utc());
    }
    if let Ok(time) = DateTime::parse_from_rfc3339(value) {
        return Ok(time.with_timezone(&Utc));
    }
    let duration = parse_duration(value).map_err(|_| {
        ScoutError::Usage(format!(
            "invalid --until {value:?}; expected YYYY-MM-DD, an RFC 3339 time, or a duration like 30d"
        ))
    })?;
    TimeDelta::from_std(duration)
        .ok()
        .and_then(|delta| now.checked_add_signed(delta))
        .ok_or_else(|| ScoutError::Usage("--until is too far in the future".into()).into())
}

fn add_reminders(
    report: &mut Report,
    store: &Store,
    issue: &IssueRef,
    now: DateTime<Utc>,
) -> Result<()> {
    for reminder in issue_reminders(store, issue, now)? {
        report.meta.remind(reminder);
    }
    Ok(())
}

fn show(ctx: &mut Context<'_>, reference: &IssueRef) -> Result<Report> {
    let services = ctx.services()?;
    let indexer = GitHubIndexer::new(
        services.gateway.clone(),
        services.store.clone(),
        services.config.clone(),
    );
    let fetched = indexer.fetch_issue(reference, ctx.freshness());
    let metrics = services.gateway.metrics();
    let fetched = fetched?;
    let store = services
        .store
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let status = store.issue_mark_status(reference, ctx.now)?;
    let mut report = Report::new(json!({
        "issue": fetched.detail,
        "issue_mark": status,
    }));
    add_reminders(&mut report, &store, reference, ctx.now)?;
    report.meta.as_of = Some(fetched.as_of);
    report.meta.requests = metrics.requests;
    report.meta.served_304 = metrics.served_304;
    for warning in &fetched.warnings {
        report.meta.warn(warning.clone());
    }
    report.meta.partial |= !fetched.warnings.is_empty();
    report.human = show_lines(&fetched.detail, status.as_ref());
    Ok(report)
}

fn show_lines(detail: &IssueDetail, status: Option<&IssueMarkStatus>) -> Vec<String> {
    let row = &detail.row;
    let mut lines = vec![
        format!("{} [{}] {}", row.issue, row.state, row.title),
        row.issue.html_url(),
    ];
    if detail.is_pull_request {
        lines.push(format!(
            "Note: {} is a pull request, not an issue; gates refuse it.",
            row.issue
        ));
    }
    if let Some(author) = &detail.author {
        lines.push(format!("Author: {author}"));
    }
    if !row.labels.is_empty() {
        lines.push(format!("Labels: {}", row.labels.join(", ")));
    }
    lines.push(format!(
        "Assignees: {}",
        if row.assignees.is_empty() {
            "none".to_owned()
        } else {
            row.assignees.join(", ")
        }
    ));
    lines.push(format!(
        "Updated: {}  Comments: {}",
        row.updated_at.format("%Y-%m-%d %H:%M UTC"),
        row.comments
    ));
    if detail.linked_prs.is_empty() {
        lines.push("Linked PRs: none".into());
    } else {
        lines.push("Linked PRs:".into());
        for pr in &detail.linked_prs {
            lines.push(format!(
                "  {} ({}, {}{})",
                pr.url,
                pr.event,
                pr.state.as_deref().unwrap_or("unknown"),
                pr.author
                    .as_deref()
                    .map(|a| format!(", by {a}"))
                    .unwrap_or_default()
            ));
        }
    }
    if let Some(status) = status {
        if status.changed_since_mark == Some(true) {
            lines.push(
                "Note: the issue changed since it was marked; re-read before relying on the mark."
                    .into(),
            );
        }
    }
    lines.push(String::new());
    lines.push(
        detail
            .body
            .clone()
            .unwrap_or_else(|| "(no description)".into()),
    );
    if !detail.comments.is_empty() {
        lines.push(String::new());
        lines.push(format!("Last {} comment(s):", detail.comments.len()));
        for comment in &detail.comments {
            lines.push(format!(
                "--- {} at {} ({})",
                comment.author.as_deref().unwrap_or("ghost"),
                comment.created_at.format("%Y-%m-%d %H:%M UTC"),
                comment.url
            ));
            lines.push(comment.body.clone());
        }
    }
    lines
}

fn mark(ctx: &mut Context<'_>, new: NewIssueMark) -> Result<Report> {
    let agent = ctx.agent()?;
    let mut store = ctx.open_store()?;
    let issue = new.issue.clone();
    let mark = mark_issue(&mut store, new, &agent, ctx.now)?;
    let status = store.issue_mark_status(&issue, ctx.now)?;
    let mut line = format!("Marked {} {}", mark.issue, mark.verdict);
    if let Some(reason) = &mark.reason {
        line.push_str(&format!(" ({reason})"));
    }
    line.push_str(&format!(": {}", mark.note));
    if let Some(expires_at) = mark.expires_at {
        line.push_str(&format!(
            " (until {})",
            expires_at.format("%Y-%m-%d %H:%M UTC")
        ));
    }
    let mut lines = vec![line];
    if mark.seen_updated_at.is_none() {
        lines.push(
            "The issue is not in the local index; no snapshot was taken (run `contrib-scout sync` to index it)."
                .into(),
        );
    }
    let mut report = Report::new(json!({ "mark": mark, "status": status }));
    add_reminders(&mut report, &store, &issue, ctx.now)?;
    report.human = lines;
    Ok(report)
}

fn unmark(ctx: &mut Context<'_>, issue: &IssueRef) -> Result<Report> {
    let agent = ctx.agent()?;
    let mut store = ctx.open_store()?;
    let removed = store.delete_issue_mark(issue, &agent, ctx.now)?;
    let line = match &removed {
        Some(mark) => format!("Removed the {} mark on {issue}", mark.verdict),
        None => format!("{issue} had no mark"),
    };
    let mut report = Report::new(json!({
        "issue": issue,
        "removed": removed.is_some(),
        "mark": removed,
    }));
    add_reminders(&mut report, &store, issue, ctx.now)?;
    report.human = vec![line];
    Ok(report)
}

fn marks(
    ctx: &mut Context<'_>,
    verdict: Option<IssueVerdict>,
    repo: Option<&RepoName>,
) -> Result<Report> {
    let store = ctx.open_store()?;
    let filter = IssueMarkFilter {
        verdict,
        repo: repo.cloned(),
        active_at: None,
    };
    let statuses = store.issue_mark_statuses(&filter, ctx.now)?;
    let mut report = Report::new(serde_json::to_value(&statuses)?);
    let repos: BTreeSet<&RepoName> = statuses
        .iter()
        .map(|status| &status.mark.issue.repo)
        .chain(repo)
        .collect();
    for repo in repos {
        let mark = store.effective_mark(repo)?;
        report
            .meta
            .remind(reminder_for(repo, mark.as_ref(), ctx.now));
    }
    report.human = if statuses.is_empty() {
        vec!["No issue marks.".into()]
    } else {
        statuses.iter().map(mark_line).collect()
    };
    Ok(report)
}

fn mark_line(status: &IssueMarkStatus) -> String {
    let mark = &status.mark;
    let mut line = format!("{} {}", mark.issue, mark.verdict);
    if let Some(reason) = &mark.reason {
        line.push_str(&format!(" ({reason})"));
    }
    line.push_str(&format!(
        " — \"{}\" ({} by {})",
        mark.note,
        mark.marked_at.format("%Y-%m-%d"),
        mark.marked_by
    ));
    if status.changed_since_mark == Some(true) {
        line.push_str(" [changed since mark]");
    }
    if !status.active {
        line.push_str(" [expired]");
    }
    line
}
