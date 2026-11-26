//! `claim list|renew|release` (§9, §10). Local only; zero requests.

use super::Context;
use crate::render::Report;
use anyhow::Result;
use clap::Subcommand;
use contrib_scout_core::claims::ClaimService;
use contrib_scout_core::coordinator::SqliteCoordinator;
use contrib_scout_core::issue_marks::issue_reminders;
use contrib_scout_core::models::Claim;
use contrib_scout_core::policy::repo_reminder;
use contrib_scout_core::IssueRef;
use serde_json::json;
use std::collections::BTreeSet;
use std::sync::{Arc, PoisonError};

#[derive(Subcommand, Debug)]
pub enum ClaimCommand {
    /// List active claims (only yours when --agent is set, unless --all).
    List {
        /// List every agent's claims even when --agent is set.
        #[arg(long)]
        all: bool,
    },
    /// Extend your lease on an issue (requires --agent).
    Renew {
        /// owner/repo#N or an issue URL
        #[arg(value_name = "REF")]
        reference: IssueRef,
    },
    /// Release your claim on an issue (requires --agent).
    Release {
        /// owner/repo#N or an issue URL
        #[arg(value_name = "REF")]
        reference: IssueRef,
    },
}

fn claim_line(claim: &Claim) -> String {
    format!(
        "{}  {}  until {}{}",
        claim.issue,
        claim.agent,
        claim.lease_until.format("%Y-%m-%d %H:%M UTC"),
        claim
            .branch
            .as_deref()
            .map(|b| format!("  branch {b}"))
            .unwrap_or_default()
    )
}

pub fn run(ctx: &mut Context<'_>, command: &ClaimCommand) -> Result<Report> {
    // Renew and release write claims: the agent id is required before anything opens.
    let agent = match command {
        ClaimCommand::List { .. } => None,
        ClaimCommand::Renew { .. } | ClaimCommand::Release { .. } => Some(ctx.agent()?),
    };
    let config = ctx.config()?;
    let store = ctx.open_store()?.shared();
    let coordinator = Arc::new(SqliteCoordinator::from_config(
        store.clone(),
        ctx.agent_or_anonymous(),
        &config,
    ));
    let claims = ClaimService::new(coordinator, store.clone(), config.claim_lease());
    let mut report = match (command, agent) {
        (ClaimCommand::List { all }, _) => {
            let filter = if *all { None } else { ctx.cli.agent.as_ref() };
            let list = claims.list(filter)?;
            let human = if list.is_empty() {
                vec!["No active claims".to_owned()]
            } else {
                list.iter().map(claim_line).collect()
            };
            let repos: BTreeSet<_> = list.iter().map(|c| c.issue.repo.clone()).collect();
            let mut report = Report::new(serde_json::to_value(&list)?).human(human);
            let guard = store.lock().unwrap_or_else(PoisonError::into_inner);
            for repo in &repos {
                report.meta.remind(repo_reminder(&guard, repo, ctx.now)?);
            }
            report
        }
        (ClaimCommand::Renew { reference }, Some(agent)) => {
            let claim = claims.renew(reference, &agent)?;
            let human = vec![format!("Renewed: {}", claim_line(&claim))];
            let mut report = Report::new(json!({ "claim": claim })).human(human);
            add_issue_reminders(&mut report, &store, reference, ctx)?;
            report
        }
        (ClaimCommand::Release { reference }, Some(agent)) => {
            let released = claims.release(reference, &agent)?;
            let human = vec![if released {
                format!("Released your claim on {reference}")
            } else {
                format!("You held no active claim on {reference}")
            }];
            let mut report = Report::new(json!({
                "issue": reference,
                "agent": agent,
                "released": released,
            }))
            .human(human);
            add_issue_reminders(&mut report, &store, reference, ctx)?;
            report
        }
        _ => unreachable!("renew and release resolved the agent above"),
    };
    report.meta.as_of = Some(ctx.now);
    Ok(report)
}

fn add_issue_reminders(
    report: &mut Report,
    store: &contrib_scout_core::store::SharedStore,
    reference: &IssueRef,
    ctx: &Context<'_>,
) -> Result<()> {
    let guard = store.lock().unwrap_or_else(PoisonError::into_inner);
    for reminder in issue_reminders(&guard, reference, ctx.now)? {
        report.meta.remind(reminder);
    }
    Ok(())
}
