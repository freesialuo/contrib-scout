//! `candidates`: local data only, zero requests (§7, §10). Step 4 owns this file.

use super::discover::{candidate_line, copy_metrics};
use super::Context;
use crate::render::Report;
use anyhow::Result;
use clap::Args;
use contrib_scout_core::discover::{CandidateQuery, Discovery, Pipeline};
use serde_json::json;

#[derive(Args, Debug)]
pub struct CandidatesArgs {
    /// Minimum score (0-100; shortlisted issues are always shown).
    #[arg(long, value_name = "N")]
    pub min_score: Option<f64>,
    /// Primary language.
    #[arg(long, value_name = "L")]
    pub lang: Option<String>,
    /// Maximum issues to return.
    #[arg(long, value_name = "N", default_value_t = 20, value_parser = clap::value_parser!(u32).range(1..=1000))]
    pub limit: u32,
    /// Do not exclude repositories marked as banning AI or skip-marked issues.
    #[arg(long)]
    pub include_marked: bool,
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
    })?;

    let mut human: Vec<String> = result.candidates.iter().map(candidate_line).collect();
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
    let mut report = Report::new(json!(result)).human(human);
    for reminder in result.reminders {
        report.meta.remind(reminder);
    }
    copy_metrics(&mut report, &services);
    Ok(report)
}
