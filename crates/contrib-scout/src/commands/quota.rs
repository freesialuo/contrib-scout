//! `quota`: shared quota and window usage (§9, §10). Step 2 owns this file.

use super::Context;
use crate::render::Report;
use anyhow::Result;
use contrib_scout_core::not_implemented;

pub fn run(_ctx: &mut Context<'_>) -> Result<Report> {
    Err(not_implemented("quota"))
}
