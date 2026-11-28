//! `policy files|show|set|unset|due` (§5, §10). The CLI serves raw text and
//! stores the agent's mark; it never judges the policy itself.

use super::{Context, Services};
use crate::render::Report;
use anyhow::Result;
use chrono::{DateTime, Utc};
use clap::{Args, Subcommand};
use contrib_scout_core::models::{
    Evidence, EvidenceSpec, PolicyMark, PolicyScope, Requirement, ScopeSetting, Stance,
};
use contrib_scout_core::policy::{
    expires_at, reminder_for, repo_reminder, GitHubPolicyFiles, PolicyFetch, PolicyMarks,
    StoreMarks,
};
use contrib_scout_core::store::{CachedPolicyFile, SharedStore, Store};
use contrib_scout_core::{Freshness, MarkTarget, RepoName, ScoutError};
use serde_json::{json, Value};
use std::sync::MutexGuard;

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
    /// Evidence (repeatable, at least one): a path listed by `policy files`
    /// (`owner/.github/PATH` for the org's file), optionally with up to 3
    /// lines to quote (`PATH:10-12`); an absent candidate path that was
    /// checked (for none_found); or a URL.
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

pub fn run(ctx: &mut Context<'_>, command: &PolicyCommand) -> Result<Report> {
    match command {
        PolicyCommand::Files { repo } => files(ctx, repo),
        PolicyCommand::Show { target } => show(ctx, target),
        PolicyCommand::Set(args) => set(ctx, args),
        PolicyCommand::Unset { target } => unset(ctx, target),
        PolicyCommand::Due => due(ctx),
    }
}

fn lock(store: &SharedStore) -> MutexGuard<'_, Store> {
    store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn usage(message: impl Into<String>) -> anyhow::Error {
    ScoutError::Usage(message.into()).into()
}

fn date(at: DateTime<Utc>) -> String {
    at.format("%Y-%m-%d").to_string()
}

fn short(commit: &str) -> &str {
    commit.get(..12).unwrap_or(commit)
}

fn copy_metrics(report: &mut Report, services: &Services) {
    let metrics = services.gateway.metrics();
    report.meta.requests = metrics.requests;
    report.meta.served_304 = metrics.served_304;
}

fn policy_service(services: &Services) -> GitHubPolicyFiles {
    GitHubPolicyFiles::new(
        services.gateway.clone(),
        services.store.clone(),
        services.config.clone(),
    )
}

// ---------------------------------------------------------------------------
// policy files
// ---------------------------------------------------------------------------

fn files(ctx: &mut Context<'_>, repo: &RepoName) -> Result<Report> {
    let services = ctx.services()?;
    let fetch = policy_service(&services).fetch_report(repo, ctx.freshness())?;
    let reminder = repo_reminder(&lock(&services.store), repo, ctx.now)?;
    let mut report = Report::new(serde_json::to_value(&fetch)?);
    copy_metrics(&mut report, &services);
    for warning in &fetch.warnings {
        report.meta.warn(warning.clone());
        report.meta.partial = true;
    }
    report.meta.remind(reminder);
    report.human = files_human(&fetch);
    Ok(report)
}

fn files_human(fetch: &PolicyFetch) -> Vec<String> {
    let mut lines = Vec::new();
    for source in &fetch.sources {
        lines.push(match &source.commit {
            Some(commit) if source.exists => format!(
                "Source {} at {} (checked {})",
                source.repo,
                short(commit),
                source.checked_at.to_rfc3339()
            ),
            _ => format!("Source {}: not found or empty", source.repo),
        });
    }
    if fetch.files.is_empty() {
        lines.push("No candidate policy files found.".into());
    }
    for file in &fetch.files {
        lines.push(String::new());
        let mut header = format!("=== {}/{} ({} bytes", file.repo, file.path, file.size);
        if file.truncated {
            header.push_str(", truncated");
        }
        header.push_str(&format!(") {}", file.url));
        lines.push(header);
        if let Some(text) = &file.text {
            lines.extend(text.lines().map(str::to_owned));
        }
    }
    if !fetch.absent.is_empty() {
        lines.push(String::new());
        let mut by_repo: Vec<(String, Vec<&str>)> = Vec::new();
        for absent in &fetch.absent {
            let repo = absent.repo.to_string();
            match by_repo.iter_mut().find(|(r, _)| *r == repo) {
                Some((_, paths)) => paths.push(&absent.path),
                None => by_repo.push((repo, vec![&absent.path])),
            }
        }
        for (repo, paths) in by_repo {
            lines.push(format!("Absent in {repo}: {}", paths.join(", ")));
        }
    }
    lines
}

// ---------------------------------------------------------------------------
// policy show
// ---------------------------------------------------------------------------

fn show(ctx: &mut Context<'_>, target: &MarkTarget) -> Result<Report> {
    let store = ctx.open_store()?;
    let mark = store.mark(target)?;
    let history = store.mark_history(target)?;
    let mut data = json!({ "target": target, "mark": mark, "history": history });
    let mut human = Vec::new();
    let mut reminder = None;
    let shown = match target {
        MarkTarget::Repo(repo) => {
            let effective = store.effective_mark(repo)?;
            let repo_reminder = reminder_for(repo, effective.as_ref(), ctx.now);
            data["effective"] = serde_json::to_value(&effective)?;
            data["reminder"] = serde_json::to_value(&repo_reminder)?;
            reminder = Some(repo_reminder);
            if mark.is_none() {
                human.push(match &effective {
                    Some(org) => {
                        format!(
                            "No mark on {target}; the org mark of {} applies.",
                            org.target
                        )
                    }
                    None => format!("No mark on {target}."),
                });
            }
            effective
        }
        MarkTarget::Org(_) => {
            if mark.is_none() {
                human.push(format!("No mark on org {target}."));
            }
            mark.clone()
        }
    };
    if let Some(shown) = &shown {
        human.extend(mark_lines(shown, ctx.now));
    }
    if !history.is_empty() {
        human.push(String::new());
        human.push(format!("History of {target} ({}):", history.len()));
        for event in &history {
            let detail = event
                .mark
                .as_ref()
                .map(|m| format!(" {} — \"{}\"", m.stance, m.note))
                .unwrap_or_default();
            human.push(format!(
                "  {} {} by {}{}",
                event.at.to_rfc3339(),
                event.action,
                event.agent,
                detail
            ));
        }
    }
    let mut report = Report::new(data).human(human);
    if let Some(reminder) = reminder {
        report.meta.remind(reminder);
    }
    Ok(report)
}

fn mark_lines(mark: &PolicyMark, now: DateTime<Utc>) -> Vec<String> {
    let mut lines = vec![
        format!(
            "{} {}: {} — \"{}\"",
            mark.target.kind(),
            mark.target,
            mark.stance.label(),
            mark.note
        ),
        format!(
            "  marked {} by {}; review due {}{}",
            date(mark.marked_at),
            mark.marked_by,
            date(mark.expires_at),
            if mark.review_due(now) { " (now)" } else { "" }
        ),
    ];
    let scope = serde_json::to_value(&mark.scope).unwrap_or(Value::Null);
    if mark.scope != PolicyScope::default() {
        lines.push(format!("  scope: {scope}"));
    }
    if !mark.requirements.is_empty() {
        let names: Vec<String> = mark
            .requirements
            .iter()
            .map(|r| serde_json::to_string(r).unwrap_or_default())
            .collect();
        lines.push(format!("  requirements: {}", names.join(", ")));
    }
    for evidence in &mark.evidence {
        let lines_part = evidence.lines.map(|l| format!(":{l}")).unwrap_or_default();
        lines.push(format!(
            "  evidence {}{} {}",
            evidence.path, lines_part, evidence.url
        ));
        for quoted in evidence.excerpt.lines() {
            lines.push(format!("    > {quoted}"));
        }
    }
    lines
}

// ---------------------------------------------------------------------------
// policy set
// ---------------------------------------------------------------------------

fn set(ctx: &mut Context<'_>, args: &PolicySetArgs) -> Result<Report> {
    let agent = ctx.agent()?;
    let services = ctx.services()?;
    let mut resolver = EvidenceResolver {
        services: &services,
        target: &args.target,
        freshness: ctx.freshness(),
        fetched: false,
        warnings: Vec::new(),
    };
    let mut evidence = Vec::new();
    for spec in &args.evidence {
        evidence.push(resolver.resolve(spec)?);
    }
    let warnings = std::mem::take(&mut resolver.warnings);

    let mut scope = PolicyScope::default();
    for setting in &args.scope {
        scope
            .apply(setting)
            .map_err(|error| usage(error.to_string()))?;
    }
    for label in &args.forbid_labels {
        if !scope.forbidden_labels.contains(label) {
            scope.forbidden_labels.push(label.clone());
        }
    }
    let mut requirements: Vec<Requirement> = Vec::new();
    for requirement in &args.requirements {
        if !requirements.contains(requirement) {
            requirements.push(requirement.clone());
        }
    }
    let mark = PolicyMark {
        target: args.target.clone(),
        stance: args.stance.clone(),
        scope,
        requirements,
        evidence,
        note: args.note.trim().to_owned(),
        marked_by: agent,
        marked_at: ctx.now,
        expires_at: expires_at(&services.config, &args.stance, ctx.now),
    };
    let marks = StoreMarks::new(services.store.clone(), services.config.clone(), ctx.now);
    marks.set(&mark)?;

    let mut report = Report::new(json!({ "mark": mark }));
    copy_metrics(&mut report, &services);
    for warning in warnings {
        report.meta.warn(warning);
    }
    let mut human = vec![format!("Recorded mark on {}.", mark.target)];
    human.extend(mark_lines(&mark, ctx.now));
    report.human = human;
    if let MarkTarget::Repo(repo) = &mark.target {
        report
            .meta
            .remind(repo_reminder(&lock(&services.store), repo, ctx.now)?);
    }
    Ok(report)
}

struct EvidenceResolver<'a> {
    services: &'a Services,
    target: &'a MarkTarget,
    freshness: Freshness,
    /// A policy-files fetch was already attempted for a cache miss.
    fetched: bool,
    warnings: Vec<String>,
}

fn is_url(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.starts_with("https://") || lower.starts_with("http://")
}

impl EvidenceResolver<'_> {
    /// The repository whose `policy files` covers this target.
    fn fetch_repo(&self) -> Result<RepoName> {
        Ok(match self.target {
            MarkTarget::Repo(repo) => repo.clone(),
            MarkTarget::Org(org) => RepoName::new(org.as_str(), ".github")?,
        })
    }

    /// `(source repository, path)` pairs a non-URL evidence path may refer to.
    fn locations(&self, path: &str) -> Result<Vec<(RepoName, String)>> {
        let owner = self.target.owner();
        let org_repo = RepoName::new(owner, ".github")?;
        let prefix = format!("{owner}/.github/");
        if let Some((head, rest)) = path.split_at_checked(prefix.len()) {
            if !rest.is_empty() && head.eq_ignore_ascii_case(&prefix) {
                return Ok(vec![(org_repo, rest.to_owned())]);
            }
        }
        let path = path.trim_start_matches("./").to_owned();
        Ok(match self.target {
            MarkTarget::Repo(repo) if repo.key() != org_repo.key() => {
                vec![(repo.clone(), path.clone()), (org_repo, path)]
            }
            _ => vec![(org_repo, path)],
        })
    }

    fn lookup(&self, path: &str) -> Result<Option<CachedPolicyFile>> {
        let store = lock(&self.services.store);
        for (repo, path) in self.locations(path)? {
            if let Some(row) = store.cached_policy_file(&repo, &path)? {
                if !row.data.path.is_empty() {
                    return Ok(Some(row.data));
                }
            }
        }
        Ok(None)
    }

    fn resolve(&mut self, spec: &EvidenceSpec) -> Result<Evidence> {
        if is_url(&spec.path) {
            let cached = lock(&self.services.store).cached_policy_file_by_url(&spec.path)?;
            if let Some(row) = cached {
                return self.evidence_from_row(spec, &row.data);
            }
            if spec.lines.is_some() {
                self.warnings.push(format!(
                    "{} is not a cached policy file; recorded its lines without an excerpt",
                    spec.path
                ));
            }
            return Ok(Evidence {
                path: spec.path.clone(),
                lines: spec.lines,
                url: spec.path.clone(),
                excerpt: String::new(),
            });
        }
        let mut row = self.lookup(&spec.path)?;
        if row.is_none() && !self.fetched {
            self.fetched = true;
            let repo = self.fetch_repo()?;
            let service = policy_service(self.services);
            match service.fetch_report(&repo, self.freshness) {
                Ok(fetch) => self.warnings.extend(fetch.warnings),
                Err(error)
                    if matches!(ScoutError::find(&error), Some(ScoutError::NotFound { .. })) => {}
                Err(error) => return Err(error),
            }
            row = self.lookup(&spec.path)?;
        }
        match row {
            Some(row) => self.evidence_from_row(spec, &row),
            None => Err(usage(format!(
                "evidence {} is not among the policy files cached for {}; run `contrib-scout policy files {}` and cite a listed or absent path (owner/.github/PATH for the org's), or a URL",
                spec.path,
                self.target,
                self.fetch_repo()?
            ))),
        }
    }

    fn evidence_from_row(&self, spec: &EvidenceSpec, row: &CachedPolicyFile) -> Result<Evidence> {
        let org_source = match self.target {
            MarkTarget::Repo(repo) => row.repo.key() != repo.key(),
            MarkTarget::Org(_) => false,
        };
        let path = if org_source {
            format!("{}/{}", row.repo, row.path)
        } else {
            row.path.clone()
        };
        let Some(file) = &row.file else {
            // An absent candidate path: records what was checked (none_found).
            if spec.lines.is_some() {
                return Err(usage(format!(
                    "{path} does not exist in {}; it can be cited without lines only",
                    row.repo
                )));
            }
            return Ok(Evidence {
                path,
                lines: None,
                url: row
                    .url
                    .clone()
                    .unwrap_or_else(|| format!("https://github.com/{}", row.repo)),
                excerpt: String::new(),
            });
        };
        let excerpt = match spec.lines {
            None => String::new(),
            Some(range) => {
                let count = (range.end - range.start + 1) as usize;
                if count > Evidence::MAX_EXCERPT_LINES {
                    return Err(usage(format!(
                        "evidence {path}:{range} spans {count} lines; quote at most {}",
                        Evidence::MAX_EXCERPT_LINES
                    )));
                }
                let Some(text) = &file.text else {
                    return Err(usage(format!(
                        "the text of {path} is not cached; run `contrib-scout policy files {}` first",
                        self.fetch_repo()?
                    )));
                };
                let lines: Vec<&str> = text.lines().collect();
                if range.end as usize > lines.len() {
                    let note = if file.truncated {
                        " (cached text is truncated)"
                    } else {
                        ""
                    };
                    return Err(usage(format!(
                        "evidence {path}:{range} is past the end of the file, which has {} lines{note}",
                        lines.len()
                    )));
                }
                lines[range.start as usize - 1..range.end as usize].join("\n")
            }
        };
        Ok(Evidence {
            path,
            lines: spec.lines,
            url: file.url.clone(),
            excerpt,
        })
    }
}

// ---------------------------------------------------------------------------
// policy unset / due
// ---------------------------------------------------------------------------

fn unset(ctx: &mut Context<'_>, target: &MarkTarget) -> Result<Report> {
    let agent = ctx.agent()?;
    let store = ctx.open_store()?.shared();
    if lock(&store).mark(target)?.is_none() {
        return Err(ScoutError::NotFound {
            what: format!("policy mark on {target}"),
        }
        .into());
    }
    let config = ctx.config()?;
    let marks = StoreMarks::new(store.clone(), config, ctx.now);
    let removed = marks.unset(target, &agent)?;
    let mut report =
        Report::new(json!({ "target": target, "removed": removed })).human(vec![format!(
            "Removed the mark on {target} (history kept)."
        )]);
    if let MarkTarget::Repo(repo) = target {
        report
            .meta
            .remind(repo_reminder(&lock(&store), repo, ctx.now)?);
    }
    Ok(report)
}

fn due(ctx: &mut Context<'_>) -> Result<Report> {
    let store = ctx.open_store()?;
    let marks = store.due_marks(ctx.now)?;
    let mut human = Vec::new();
    if marks.is_empty() {
        human.push("No marks are due for review.".into());
    }
    let mut report = Report::new(serde_json::to_value(&marks)?);
    for mark in &marks {
        match &mark.target {
            // The renderer prints repo reminders prominently; no duplicate line.
            MarkTarget::Repo(repo) => report.meta.remind(reminder_for(repo, Some(mark), ctx.now)),
            MarkTarget::Org(_) => human.push(format!(
                "⚠ org {}: {} — \"{}\" (marked {}) [review due]",
                mark.target,
                mark.stance.label(),
                mark.note,
                date(mark.marked_at)
            )),
        }
    }
    if !marks.is_empty() {
        human.push(format!("{} mark(s) due for review.", marks.len()));
    }
    report.human = human;
    Ok(report)
}
