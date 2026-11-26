//! `candidates`: local data only, zero requests (§7, §10). Step 4 owns this file.

use super::Context;
use crate::render::Report;
use anyhow::Result;
use clap::Args;
use contrib_scout_core::not_implemented;

#[derive(Args, Debug)]
pub struct CandidatesArgs {
    /// Minimum score.
    #[arg(long, value_name = "N")]
    pub min_score: Option<f64>,
    /// Primary language.
    #[arg(long, value_name = "L")]
    pub lang: Option<String>,
    /// Maximum issues to return.
    #[arg(long, value_name = "N", default_value_t = 20, value_parser = clap::value_parser!(u32).range(1..=1000))]
    pub limit: u32,
    /// Do not exclude repositories marked as banning AI.
    #[arg(long)]
    pub include_marked: bool,
}

pub fn run(_ctx: &mut Context<'_>, _args: &CandidatesArgs) -> Result<Report> {
    Err(not_implemented("candidates"))
}
