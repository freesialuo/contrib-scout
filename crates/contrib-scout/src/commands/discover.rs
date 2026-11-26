//! `discover` (§7, §10). Step 4 owns this file.

use super::Context;
use crate::render::{Exit, Report};
use anyhow::Result;
use clap::Args;
use contrib_scout_core::discover::{
    Candidate, DiscoverQuery, Discovery, Pipeline, Stage, CANDIDATES_PER_REPO,
};
use serde_json::json;
use std::collections::BTreeMap;

#[derive(Args, Debug)]
pub struct DiscoverArgs {
    /// Primary language, e.g. rust.
    #[arg(long, value_name = "L")]
    pub lang: Option<String>,
    /// GitHub topic (checked against repository metadata, no extra request).
    #[arg(long, value_name = "T")]
    pub topic: Option<String>,
    /// Issue label to look for (repeatable, OR-ed; default "good first issue").
    #[arg(long = "label", value_name = "L")]
    pub labels: Vec<String>,
    /// Maximum repositories to return.
    #[arg(long, value_name = "N", default_value_t = 20, value_parser = clap::value_parser!(u32).range(1..=1000))]
    pub limit: u32,
    /// Print the plan and an estimated request count without fetching anything.
    #[arg(long)]
    pub explain: bool,
    /// Do not exclude repositories marked as banning AI or skip-marked issues.
    #[arg(long)]
    pub include_marked: bool,
}

pub(crate) fn candidate_line(candidate: &Candidate) -> String {
    let row = &candidate.issue;
    let mut line = format!(
        "  {}  {:.1}  {}",
        row.issue,
        candidate.score,
        row.title.replace(['\n', '\r'], " ")
    );
    if !row.labels.is_empty() {
        line.push_str(&format!("  [{}]", row.labels.join(", ")));
    }
    if let Some(mark) = &candidate.mark {
        line.push_str(&format!("  ({}: {})", mark.verdict, mark.note));
        if mark.changed_since_mark {
            line.push_str(" [changed since mark]");
        }
    }
    line
}

pub fn run(ctx: &mut Context<'_>, args: &DiscoverArgs) -> Result<Report> {
    let services = ctx.services()?;
    let pipeline = Pipeline::new(
        services.gateway.clone(),
        services.store.clone(),
        services.config.clone(),
    );
    let query = DiscoverQuery {
        language: args.lang.clone(),
        topic: args.topic.clone(),
        labels: args.labels.clone(),
        limit: args.limit,
        explain: args.explain,
        include_marked: args.include_marked,
    };
    let mode = ctx.freshness();

    if args.explain {
        let plan = pipeline.plan(&query, mode)?;
        let mut human = vec![
            format!("Plan for: {}", plan.query),
            format!(
                "Estimated requests: at most {} (budget {})",
                plan.estimated_requests,
                ctx.max_requests(&services.config)
            ),
        ];
        for stage in &plan.stages {
            human.push(format!(
                "  {:?}: {} — {} request(s)",
                stage.stage, stage.description, stage.estimated_requests
            ));
        }
        if plan.search_cached {
            human.push(format!(
                "{} repositories known from the cached search run",
                plan.known_repos.len()
            ));
        }
        let mut report = Report::new(serde_json::to_value(&plan)?).human(human);
        for reminder in plan.reminders {
            report.meta.remind(reminder);
        }
        copy_metrics(&mut report, &services);
        return Ok(report);
    }

    let result = pipeline.discover(&query, mode)?;
    let mut human = Vec::new();
    if result.repos.is_empty() {
        human.push("No repositories found.".to_owned());
    }
    for repo in &result.repos {
        let profile = &repo.profile;
        let mut line = format!("{}  {:.1}  ★{}", profile.repo, repo.score, profile.stars);
        if let Some(language) = &profile.language {
            line.push_str(&format!("  {language}"));
        }
        if let Some(license) = &profile.license {
            line.push_str(&format!("  {license}"));
        }
        human.push(line);
        human.extend(repo.candidates.iter().map(candidate_line));
        if repo.candidates.len() == CANDIDATES_PER_REPO {
            human.push(format!(
                "  (showing the top {CANDIDATES_PER_REPO}; run `contrib-scout candidates` for all)"
            ));
        }
    }
    if result.excluded_marked > 0 {
        human.push(format!(
            "Excluded {} repositories marked Bans (use --include-marked to show them)",
            result.excluded_marked
        ));
    }
    if result.excluded_skipped > 0 {
        human.push(format!(
            "Excluded {} skip-marked issues (use --include-marked to show them)",
            result.excluded_skipped
        ));
    }
    let mut by_stage: BTreeMap<String, u32> = BTreeMap::new();
    for exclusion in result
        .excluded
        .iter()
        .filter(|e| e.stage != Stage::PolicyMark)
    {
        *by_stage
            .entry(format!("{:?}", exclusion.stage))
            .or_default() += 1;
    }
    if !by_stage.is_empty() {
        let parts: Vec<String> = by_stage.iter().map(|(s, n)| format!("{s} {n}")).collect();
        human.push(format!(
            "Dropped by stage: {} (see --json for reasons)",
            parts.join(", ")
        ));
    }

    let mut report = Report::new(json!(result)).human(human);
    report.meta.partial = result.partial;
    for warning in &result.warnings {
        report.meta.warn(warning.clone());
    }
    for reminder in result.reminders {
        report.meta.remind(reminder);
    }
    if result.new_repos > 0 {
        report.exit = Exit::Changes;
    }
    copy_metrics(&mut report, &services);
    Ok(report)
}

pub(crate) fn copy_metrics(report: &mut Report, services: &super::Services) {
    let metrics = services.gateway.metrics();
    report.meta.requests = metrics.requests;
    report.meta.served_304 = metrics.served_304;
}
