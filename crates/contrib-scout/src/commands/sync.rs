//! `sync`: incremental issue index refresh (§7, §9, §10). Step 5 owns this file.
//! The request cap is the global `--max-requests`.

use super::Context;
use crate::render::Report;
use anyhow::Result;
use clap::Args;
use contrib_scout_core::not_implemented;

#[derive(Args, Debug)]
pub struct SyncArgs {
    /// Sync every tracked repository (for the scheduled task).
    #[arg(long)]
    pub all_tracked: bool,
}

pub fn run(_ctx: &mut Context<'_>, _args: &SyncArgs) -> Result<Report> {
    Err(not_implemented("sync"))
}
