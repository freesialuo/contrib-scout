//! `sync`: incremental issue index refresh (§7, §9, §10). Step 5 owns this file.
//! The request cap is the global `--max-requests`, shared by every repository
//! in one invocation. Exit code 10 when any new or changed issue was indexed.

use super::Context;
use crate::render::{Exit, Report};
use anyhow::Result;
use clap::Args;
use contrib_scout_core::index::{GitHubIndexer, IssueIndexer, SyncReport};
use contrib_scout_core::policy::reminder_for;
use contrib_scout_core::{RepoName, ScoutError};
use serde_json::json;
use std::sync::PoisonError;

#[derive(Args, Debug)]
pub struct SyncArgs {
    /// Sync every tracked repository (for the scheduled task).
    #[arg(long, conflicts_with = "repos")]
    pub all_tracked: bool,
    /// Repositories to sync (owner/repo); required unless --all-tracked.
    #[arg(value_name = "OWNER/REPO")]
    pub repos: Vec<RepoName>,
}

pub fn run(ctx: &mut Context<'_>, args: &SyncArgs) -> Result<Report> {
    if !args.all_tracked && args.repos.is_empty() {
        return Err(ScoutError::Usage(
            "name repositories to sync (owner/repo ...) or pass --all-tracked".into(),
        )
        .into());
    }
    let services = ctx.services()?;
    let repos: Vec<RepoName> = if args.all_tracked {
        let store = services
            .store
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        store
            .tracked_repos()?
            .into_iter()
            .map(|entry| entry.repo)
            .collect()
    } else {
        let mut seen = std::collections::BTreeSet::new();
        args.repos
            .iter()
            .filter(|repo| seen.insert(repo.key()))
            .cloned()
            .collect()
    };
    let indexer = GitHubIndexer::new(
        services.gateway.clone(),
        services.store.clone(),
        services.config.clone(),
    );
    let freshness = ctx.freshness();

    let mut reports: Vec<SyncReport> = Vec::new();
    let mut failures = Vec::new();
    let mut first_error = None;
    for repo in &repos {
        match indexer.sync(repo, freshness) {
            Ok(report) => reports.push(report),
            Err(error) => {
                failures.push(json!({
                    "repo": repo,
                    "error": format!("{error:#}"),
                }));
                first_error.get_or_insert(error);
            }
        }
    }
    // Every repository failed: report the failure itself (exit 1).
    if let (Some(error), true) = (first_error, reports.is_empty()) {
        return Err(error);
    }

    let mut report = Report::new(json!({ "repos": reports, "failed": failures }));
    {
        let store = services
            .store
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        for repo in &repos {
            let mark = store.effective_mark(repo)?;
            report
                .meta
                .remind(reminder_for(repo, mark.as_ref(), ctx.now));
        }
    }
    let metrics = services.gateway.metrics();
    report.meta.requests = metrics.requests;
    report.meta.served_304 = metrics.served_304;
    for sync in &reports {
        report.meta.partial |= sync.partial;
        for warning in &sync.warnings {
            report.meta.warn(warning.clone());
        }
    }
    for failure in &failures {
        report.meta.partial = true;
        report.meta.warn(format!(
            "sync of {} failed: {}",
            failure["repo"].as_str().unwrap_or_default(),
            failure["error"].as_str().unwrap_or_default()
        ));
    }
    if reports.iter().any(SyncReport::has_changes) {
        report.exit = Exit::Changes;
    }

    let mut lines: Vec<String> = reports.iter().map(sync_line).collect();
    if repos.is_empty() {
        lines.push(
            "No tracked repositories; add some with `contrib-scout track add owner/repo`.".into(),
        );
    }
    report.human = lines;
    Ok(report)
}

fn sync_line(sync: &SyncReport) -> String {
    use contrib_scout_core::index::SyncSkip;
    match sync.skipped {
        Some(SyncSkip::Offline) => format!("{}: skipped (offline)", sync.repo),
        Some(SyncSkip::Fresh) => format!("{}: up to date (synced within the TTL)", sync.repo),
        Some(SyncSkip::Locked) => format!("{}: skipped (another agent is syncing)", sync.repo),
        None => {
            let mut line = format!(
                "{}: {} new, {} updated, {} unchanged, {} PRs skipped ({} request{})",
                sync.repo,
                sync.new_issues,
                sync.updated - sync.new_issues,
                sync.unchanged,
                sync.skipped_prs,
                sync.requests,
                if sync.requests == 1 { "" } else { "s" }
            );
            if sync.partial {
                line.push_str(" [partial: run sync again to resume]");
            }
            line
        }
    }
}
