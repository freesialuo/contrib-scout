//! `policy files|show|set|unset|due` (§5, §10). Step 3 owns this file.

use super::Context;
use crate::render::Report;
use anyhow::Result;
use clap::{Args, Subcommand};
use contrib_scout_core::models::{EvidenceSpec, Requirement, ScopeSetting, Stance};
use contrib_scout_core::{not_implemented, MarkTarget, RepoName};

#[derive(Subcommand, Debug)]
pub enum PolicyCommand {
    /// Fetch raw candidate policy files: org .github, then the repository. No interpretation.
    Files {
        /// owner/repo
        repo: RepoName,
    },
    /// Show the current mark and its history.
    Show {
        /// owner/repo or org
        target: MarkTarget,
    },
    /// Record the agent's reading of the policy (overwrites; history is kept).
    Set(PolicySetArgs),
    /// Remove a mark (history is kept).
    Unset {
        /// owner/repo or org
        target: MarkTarget,
    },
    /// List marks that are due for review.
    Due,
}

#[derive(Args, Debug)]
pub struct PolicySetArgs {
    /// owner/repo or org
    pub target: MarkTarget,
    /// bans, restricts, disclose, allows, welcomes, or none_found.
    #[arg(long, value_name = "S")]
    pub stance: Stance,
    /// Policy file path or URL, optionally with lines (repeatable, at least one).
    #[arg(long = "evidence", value_name = "PATH[:LINES]", required = true)]
    pub evidence: Vec<EvidenceSpec>,
    /// One-line summary shown verbatim in reminders.
    #[arg(long, value_name = "T")]
    pub note: String,
    /// Issue label that must not be worked with AI (repeatable).
    #[arg(long = "forbid-label", value_name = "L")]
    pub forbid_labels: Vec<String>,
    /// PR requirement (repeatable): disclose_in_pr, assisted_by_trailer,
    /// human_written_description, human_written_replies,
    /// maintainer_approval_on_issue, label_pr=LABEL.
    #[arg(long = "require", value_name = "R")]
    pub requirements: Vec<Requirement>,
    /// Scope restriction KEY=VALUE, e.g. pr_text=human_written (repeatable).
    #[arg(long = "scope", value_name = "K=V")]
    pub scope: Vec<ScopeSetting>,
}

pub fn run(_ctx: &mut Context<'_>, command: &PolicyCommand) -> Result<Report> {
    match command {
        PolicyCommand::Files { .. } => Err(not_implemented("policy files")),
        PolicyCommand::Show { .. } => Err(not_implemented("policy show")),
        PolicyCommand::Set(_) => Err(not_implemented("policy set")),
        PolicyCommand::Unset { .. } => Err(not_implemented("policy unset")),
        PolicyCommand::Due => Err(not_implemented("policy due")),
    }
}
