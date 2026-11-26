//! `repo show` (§10). Step 4 owns this file.

use super::discover::copy_metrics;
use super::Context;
use crate::render::Report;
use anyhow::Result;
use clap::Subcommand;
use contrib_scout_core::models::Stance;
use contrib_scout_core::policy::reminder_for;
use contrib_scout_core::profile::{GitHubProfiler, RepoProfiler};
use contrib_scout_core::{Freshness, RepoName, ScoutError};
use serde_json::json;

#[derive(Subcommand, Debug)]
pub enum RepoCommand {
    /// Metadata, activity, and policy mark (use --fresh to ignore the TTL).
    Show {
        /// owner/repo
        repo: RepoName,
    },
}

pub fn run(ctx: &mut Context<'_>, command: &RepoCommand) -> Result<Report> {
    match command {
        RepoCommand::Show { repo } => show(ctx, repo),
    }
}

fn show(ctx: &mut Context<'_>, repo: &RepoName) -> Result<Report> {
    let services = ctx.services()?;
    let mode = ctx.freshness();
    let mark = services
        .store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .effective_mark(repo)?;
    let reminder = reminder_for(repo, mark.as_ref(), ctx.now);
    let profiler = GitHubProfiler::new(
        services.gateway.clone(),
        services.store.clone(),
        services.config.clone(),
    );

    let mut warnings = Vec::new();
    let mut profile = profiler.profile(repo, mode)?;
    let bans = mark.as_ref().is_some_and(|m| m.stance == Stance::Bans);
    // Activity is the most expensive data; never computed for Bans repos (§7).
    if !bans {
        match profiler.activity(repo, mode) {
            Ok(activity) => profile.activity = Some(activity),
            Err(error)
                if mode == Freshness::Offline
                    && matches!(ScoutError::find(&error), Some(ScoutError::NotFound { .. })) => {}
            Err(error) => warnings.push(format!("activity for {repo} unavailable: {error:#}")),
        }
    }
    warnings.extend(profiler.take_warnings());

    let mut human = vec![format!(
        "{}{}{}",
        profile.repo,
        if profile.archived { "  [archived]" } else { "" },
        if profile.fork { "  [fork]" } else { "" }
    )];
    if let Some(description) = &profile.description {
        human.push(format!("  {}", description.replace(['\n', '\r'], " ")));
    }
    human.push(format!(
        "  stars {}  language {}  license {}  default branch {}",
        profile.stars,
        profile.language.as_deref().unwrap_or("-"),
        profile.license.as_deref().unwrap_or("-"),
        profile.default_branch.as_deref().unwrap_or("-"),
    ));
    if let Some(pushed) = profile.pushed_at {
        human.push(format!("  pushed {}", pushed.format("%Y-%m-%d")));
    }
    if !profile.topics.is_empty() {
        human.push(format!("  topics {}", profile.topics.join(", ")));
    }
    match &profile.activity {
        Some(activity) => {
            human.push(format!(
                "  external PR merge rate {} (n={})",
                activity
                    .external_pr_merge_rate
                    .map_or("-".to_owned(), |r| format!("{:.0}%", r * 100.0)),
                activity.sample_size
            ));
            human.push(format!(
                "  median maintainer first response {}",
                activity
                    .median_first_response_secs
                    .map_or("-".to_owned(), |s| format!("{:.1}h", s as f64 / 3600.0))
            ));
            human.push(format!(
                "  maintainer last active {}  (computed {})",
                activity
                    .maintainer_last_active
                    .map_or("-".to_owned(), |t| t.format("%Y-%m-%d").to_string()),
                activity.computed_at.format("%Y-%m-%d")
            ));
        }
        None if bans => human.push("  activity not computed (repository marked Bans)".into()),
        None => human.push("  activity not cached".into()),
    }

    let mut report = Report::new(json!({
        "profile": profile,
        "policy_mark": mark,
    }))
    .human(human);
    report.meta.remind(reminder);
    if !warnings.is_empty() {
        report.meta.partial = true;
        for warning in warnings {
            report.meta.warn(warning);
        }
    }
    copy_metrics(&mut report, &services);
    Ok(report)
}
