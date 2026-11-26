//! `config get|set`, implemented in step 1.

use super::Context;
use crate::render::Report;
use anyhow::Result;
use clap::Subcommand;
use serde_json::json;

#[derive(Subcommand, Debug)]
pub enum ConfigCommand {
    /// Print one configuration value.
    Get {
        /// Configuration key, e.g. claim_lease.
        key: String,
    },
    /// Change one configuration value.
    Set {
        /// Configuration key, e.g. claim_lease.
        key: String,
        /// New value, e.g. 24h.
        value: String,
    },
}

pub fn run(ctx: &mut Context<'_>, command: &ConfigCommand) -> Result<Report> {
    let file = ctx.config_file()?;
    let (key, value) = match command {
        ConfigCommand::Get { key } => (key, file.load()?.get(key)?),
        ConfigCommand::Set { key, value } => (
            key,
            file.update(|config| {
                config.set(key, value)?;
                config.get(key)
            })?,
        ),
    };
    let human = vec![format!("{key} = {value}")];
    Ok(Report::new(json!({ "key": key, "value": value })).human(human))
}
