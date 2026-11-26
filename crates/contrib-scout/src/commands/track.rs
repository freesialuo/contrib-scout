//! `track add|rm|list`: the scope refreshed by scheduled sync (§9, §10). Step 5 owns this file.

use super::Context;
use crate::render::Report;
use anyhow::Result;
use clap::Subcommand;
use contrib_scout_core::policy::reminder_for;
use contrib_scout_core::store::Store;
use contrib_scout_core::RepoName;
use serde_json::json;

#[derive(Subcommand, Debug)]
pub enum TrackCommand {
    /// Track a repository.
    Add {
        /// owner/repo
        repo: RepoName,
    },
    /// Stop tracking a repository.
    Rm {
        /// owner/repo
        repo: RepoName,
    },
    /// List tracked repositories.
    List,
}

fn remind(report: &mut Report, store: &Store, repo: &RepoName, ctx: &Context<'_>) -> Result<()> {
    let mark = store.effective_mark(repo)?;
    report
        .meta
        .remind(reminder_for(repo, mark.as_ref(), ctx.now));
    Ok(())
}

pub fn run(ctx: &mut Context<'_>, command: &TrackCommand) -> Result<Report> {
    let mut store = ctx.open_store()?;
    match command {
        TrackCommand::Add { repo } => {
            let agent = ctx.cli.agent.clone();
            let added = store.add_tracked(repo, agent.as_ref(), ctx.now)?;
            let mut report = Report::new(json!({ "repo": repo, "added": added }));
            remind(&mut report, &store, repo, ctx)?;
            report.human = vec![if added {
                format!("Tracking {repo}")
            } else {
                format!("{repo} is already tracked")
            }];
            Ok(report)
        }
        TrackCommand::Rm { repo } => {
            let removed = store.remove_tracked(repo)?;
            let mut report = Report::new(json!({ "repo": repo, "removed": removed }));
            remind(&mut report, &store, repo, ctx)?;
            report.human = vec![if removed {
                format!("Stopped tracking {repo}")
            } else {
                format!("{repo} was not tracked")
            }];
            Ok(report)
        }
        TrackCommand::List => {
            let tracked = store.tracked_repos()?;
            let mut report = Report::new(serde_json::to_value(&tracked)?);
            for entry in &tracked {
                remind(&mut report, &store, &entry.repo, ctx)?;
            }
            report.human = if tracked.is_empty() {
                vec!["No tracked repositories.".into()]
            } else {
                tracked
                    .iter()
                    .map(|entry| {
                        format!(
                            "{}  (added {}{})",
                            entry.repo,
                            entry.added_at.format("%Y-%m-%d"),
                            entry
                                .added_by
                                .as_ref()
                                .map(|by| format!(" by {by}"))
                                .unwrap_or_default()
                        )
                    })
                    .collect()
            };
            Ok(report)
        }
    }
}
