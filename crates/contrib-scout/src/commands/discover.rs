//! `discover` (§7, §10). Step 4 owns this file.

use super::Context;
use crate::render::Report;
use anyhow::Result;
use clap::Args;
use contrib_scout_core::not_implemented;

#[derive(Args, Debug)]
pub struct DiscoverArgs {
    /// Primary language, e.g. rust.
    #[arg(long, value_name = "L")]
    pub lang: Option<String>,
    /// GitHub topic.
    #[arg(long, value_name = "T")]
    pub topic: Option<String>,
    /// Issue label to look for (repeatable).
    #[arg(long = "label", value_name = "L")]
    pub labels: Vec<String>,
    /// Maximum repositories to return.
    #[arg(long, value_name = "N", default_value_t = 20, value_parser = clap::value_parser!(u32).range(1..=1000))]
    pub limit: u32,
    /// Explain why each repository was kept or dropped.
    #[arg(long)]
    pub explain: bool,
    /// Do not exclude repositories marked as banning AI.
    #[arg(long)]
    pub include_marked: bool,
}

pub fn run(_ctx: &mut Context<'_>, _args: &DiscoverArgs) -> Result<Report> {
    Err(not_implemented("discover"))
}
