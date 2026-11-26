//! `gate start|pr`: always live, fail closed (§8, §10).
//! Exit 0 pass, 20 blocked, 1 error. The agent id comes from the global `--agent`
//! (required). `--offline` is refused; `--fresh` changes nothing (gates are always live).

use super::Context;
use crate::render::{Exit, Report};
use anyhow::Result;
use clap::Subcommand;
use contrib_scout_core::coordinator::SqliteCoordinator;
use contrib_scout_core::gate::{Gatekeeper, LiveGatekeeper, PrOptions};
use contrib_scout_core::issue_marks::issue_reminders;
use contrib_scout_core::models::{GateKind, GateReport, GateVerdict};
use contrib_scout_core::{IssueRef, ScoutError};
use std::sync::{Arc, PoisonError};

#[derive(Subcommand, Debug)]
pub enum GateCommand {
    /// Live checks before starting work; acquires or renews the claim on pass.
    Start {
        /// owner/repo#N or an issue URL
        #[arg(value_name = "REF")]
        reference: IssueRef,
    },
    /// Live checks before opening a PR; returns full policy text and PR requirements.
    Pr {
        /// owner/repo#N or an issue URL
        #[arg(value_name = "REF")]
        reference: IssueRef,
        /// Your PR head branch, excluded from the linked-PR check and recorded on the claim.
        #[arg(long, value_name = "B")]
        branch: Option<String>,
        /// Where a maintainer approved the work (https URL), when the mark requires approval.
        #[arg(long, value_name = "URL")]
        approval_url: Option<String>,
    },
}

pub fn run(ctx: &mut Context<'_>, command: &GateCommand) -> Result<Report> {
    let agent = ctx.agent()?;
    if ctx.cli.offline {
        return Err(ScoutError::LiveRequired {
            what: "gate".into(),
        }
        .into());
    }
    let (reference, kind, options) = match command {
        GateCommand::Start { reference } => (reference, GateKind::Start, PrOptions::default()),
        GateCommand::Pr {
            reference,
            branch,
            approval_url,
        } => {
            if let Some(url) = approval_url {
                if !url.starts_with("https://") || url.len() <= "https://".len() {
                    return Err(ScoutError::Usage(format!(
                        "--approval-url must be an https URL, got {url:?}"
                    ))
                    .into());
                }
            }
            if branch.as_deref().is_some_and(|b| b.trim().is_empty()) {
                return Err(ScoutError::Usage("--branch must not be empty".into()).into());
            }
            (
                reference,
                GateKind::Pr,
                PrOptions {
                    branch: branch.clone(),
                    approval_url: approval_url.clone(),
                },
            )
        }
    };

    let services = ctx.services()?;
    let coordinator = Arc::new(SqliteCoordinator::from_config(
        services.store.clone(),
        agent.clone(),
        &services.config,
    ));
    let gatekeeper = LiveGatekeeper::new(
        services.gateway.clone(),
        services.store.clone(),
        services.config.clone(),
        coordinator,
    );
    let result = match kind {
        GateKind::Start => gatekeeper.start(reference, &agent),
        GateKind::Pr => gatekeeper.pr(reference, &agent, &options),
    };
    let metrics = services.gateway.metrics();
    let gate = result?;

    let mut report = Report::new(serde_json::to_value(&gate)?);
    {
        let store = services
            .store
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        for reminder in issue_reminders(&store, reference, ctx.now)? {
            report.meta.remind(reminder);
        }
    }
    report.meta.as_of = Some(gate.as_of);
    report.meta.requests = metrics.requests;
    report.meta.served_304 = metrics.served_304;
    if gate.verdict == GateVerdict::Blocked {
        report.exit = Exit::Blocked;
    }
    report.human = human_lines(&gate);
    Ok(report)
}

fn human_lines(gate: &GateReport) -> Vec<String> {
    let verdict = match gate.verdict {
        GateVerdict::Pass => "PASS",
        GateVerdict::Blocked => "BLOCKED",
        GateVerdict::Error => "ERROR",
    };
    let mut lines = vec![format!(
        "gate {} {}: {verdict} (agent {}{})",
        gate.kind.as_str(),
        gate.issue,
        gate.agent,
        gate.viewer
            .as_deref()
            .map(|v| format!(", GitHub user {v}"))
            .unwrap_or_default()
    )];
    for finding in &gate.blocked_by {
        lines.push(format!("  ✗ {}: {}", finding.reason, finding.detail));
    }
    for observation in &gate.observations {
        let url = observation
            .url
            .as_deref()
            .map(|u| format!(" <{u}>"))
            .unwrap_or_default();
        lines.push(format!(
            "  • [{}] {}: {}{url}",
            observation.severity, observation.kind, observation.detail
        ));
    }
    match &gate.claim {
        Some(claim) => lines.push(format!(
            "Claim: held by {} until {}{}",
            claim.agent,
            claim.lease_until.format("%Y-%m-%d %H:%M UTC"),
            claim
                .branch
                .as_deref()
                .map(|b| format!(" (branch {b})"))
                .unwrap_or_default()
        )),
        None => lines.push("Claim: not held".into()),
    }
    if !gate.requirements.is_empty() {
        let label = match gate.kind {
            GateKind::Start => "PR requirements (preview)",
            GateKind::Pr => "PR requirements",
        };
        lines.push(format!("{label}:"));
        for requirement in &gate.requirements {
            lines.push(format!(
                "  - {}",
                contrib_scout_core::gate::requirement_text(requirement)
            ));
        }
    }
    if !gate.checklist.is_empty() {
        lines.push("Checklist before opening the PR:".into());
        for item in &gate.checklist {
            lines.push(format!("  [ ] {item}"));
        }
    }
    let raw = &gate.raw;
    lines.push("Raw data (read it in full with --json):".into());
    if let Some(issue) = &raw.issue {
        lines.push(format!(
            "  data.raw.issue: body ({} chars), {} recent comments, {} linked PRs",
            issue.body.as_deref().map_or(0, |b| b.chars().count()),
            issue.comments.len(),
            issue.linked_prs.len()
        ));
        for pr in &issue.linked_prs {
            lines.push(format!(
                "    PR {} ({})",
                pr.url,
                pr.state.as_deref().unwrap_or("unknown")
            ));
        }
    }
    if let Some(mark) = &raw.policy_mark {
        lines.push(format!(
            "  data.raw.policy_mark: {} on {} — \"{}\"",
            mark.stance, mark.target, mark.note
        ));
        for evidence in &mark.evidence {
            lines.push(format!("    evidence {}: {}", evidence.path, evidence.url));
        }
    }
    let text = match gate.kind {
        GateKind::Start => "list only",
        GateKind::Pr => "full text",
    };
    lines.push(format!(
        "  data.raw.policy_files ({text}): {} file(s)",
        raw.policy_files.len()
    ));
    for file in &raw.policy_files {
        lines.push(format!("    {}/{}: {}", file.repo, file.path, file.url));
    }
    lines
}
