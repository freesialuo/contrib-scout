//! `candidates`: local data only, zero requests (§7, §10). Step 4 owns this file.

use super::discover::{candidate_line, copy_metrics};
use super::Context;
use crate::render::Report;
use anyhow::Result;
use chrono::SecondsFormat;
use clap::Args;
use contrib_scout_core::discover::{Candidate, CandidateQuery, Discovery, Pipeline};
use contrib_scout_core::RepoName;
use serde_json::json;

#[derive(Args, Debug)]
pub struct CandidatesArgs {
    /// Minimum score (0-100; shortlisted issues are always shown).
    #[arg(long, value_name = "N", value_parser = parse_min_score)]
    pub min_score: Option<f64>,
    /// Primary language.
    #[arg(long, value_name = "L")]
    pub lang: Option<String>,
    /// Only this repository (repeatable; an issue in any of them matches).
    #[arg(long = "repo", value_name = "OWNER/REPO")]
    pub repos: Vec<RepoName>,
    /// Only issues with this label, case-insensitive (repeatable; the issue must have all of them).
    #[arg(long = "label", value_name = "L", value_parser = parse_label)]
    pub labels: Vec<String>,
    /// Maximum issues to return.
    #[arg(long, value_name = "N", default_value_t = 20, value_parser = clap::value_parser!(u32).range(1..=1000))]
    pub limit: u32,
    /// Do not exclude repositories marked as banning AI or skip-marked issues.
    #[arg(long)]
    pub include_marked: bool,
    /// Also show issues with an active claim held by another agent (without
    /// --agent, every claimed issue is hidden by default).
    #[arg(long)]
    pub include_claimed: bool,
}

/// `--min-score`: a finite number in 0-100.
fn parse_min_score(value: &str) -> Result<f64, String> {
    let score: f64 = value.parse().map_err(|e| format!("{e}"))?;
    if score.is_finite() && (0.0..=100.0).contains(&score) {
        Ok(score)
    } else {
        Err("expected a number from 0 to 100".into())
    }
}

/// `--label`: a non-empty label name.
fn parse_label(value: &str) -> Result<String, String> {
    let label = value.trim();
    if label.is_empty() {
        Err("expected a label name".into())
    } else {
        Ok(label.to_owned())
    }
}

fn line(candidate: &Candidate) -> String {
    let mut line = candidate_line(candidate);
    if let Some(claim) = &candidate.claim {
        line.push_str(&format!(
            "  (claimed by {} until {})",
            claim.agent,
            claim.lease_until.to_rfc3339_opts(SecondsFormat::Secs, true)
        ));
    }
    line
}

pub fn run(ctx: &mut Context<'_>, args: &CandidatesArgs) -> Result<Report> {
    let services = ctx.services()?;
    let pipeline = Pipeline::new(
        services.gateway.clone(),
        services.store.clone(),
        services.config.clone(),
    );
    let result = pipeline.candidates(&CandidateQuery {
        min_score: args.min_score,
        language: args.lang.clone(),
        limit: args.limit,
        include_marked: args.include_marked,
        agent: ctx.cli.agent.clone(),
        include_claimed: args.include_claimed,
        repos: args.repos.clone(),
        labels: args.labels.clone(),
    })?;

    let mut human: Vec<String> = result.candidates.iter().map(line).collect();
    if human.is_empty() {
        human.push(
            "No candidates in the local index (run `contrib-scout discover` or `contrib-scout sync`)."
                .into(),
        );
    }
    if result.excluded_marked > 0 {
        human.push(format!(
            "Excluded {} issues in repositories marked Bans (use --include-marked to show them)",
            result.excluded_marked
        ));
    }
    if result.excluded_skipped > 0 {
        human.push(format!(
            "Excluded {} skip-marked issues (use --include-marked to show them)",
            result.excluded_skipped
        ));
    }
    if result.excluded_claimed > 0 {
        human.push(format!(
            "Excluded {} issues claimed by other agents (use --include-claimed to show them)",
            result.excluded_claimed
        ));
    }
    let mut report = Report::new(json!(result)).human(human);
    for reminder in result.reminders {
        report.meta.remind(reminder);
    }
    copy_metrics(&mut report, &services);
    Ok(report)
}
