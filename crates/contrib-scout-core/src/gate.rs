//! Live gates: always live, fail closed, raw data out (§8).
//!
//! Type-level rule (§4): a [`Gatekeeper`] takes no [`crate::Freshness`]. Its
//! GitHub inputs are fetched through the `*_live` methods, which take a
//! [`crate::LiveRead`] and return [`crate::Live<T>`]; [`GateInputs`] only holds
//! `Live<T>` for GitHub data, so cached data cannot reach [`evaluate`].
//!
//! Request plan of one gate (one shared [`Gateway`]):
//! 1. `GET /user` (uncached) for the authenticated login: "assigned to me" and
//!    "my own PR" detection. Never stored.
//! 2. `GET /repos/{o}/{r}` (archived).
//! 3. The issue, its last `gate_comments` comments (at most 2 pages), and its
//!    timeline pages (linked PRs), plus one GraphQL query for the issue's
//!    closing references (`closedByPullRequestsReferences`, open and closed):
//!    PRs linked in the Development sidebar can be missing from the timeline
//!    or appear only as a bare `connected` subject without author.
//! 4. Policy files: one `commits/HEAD` check per source (`{owner}/.github` and
//!    the repository), plus GraphQL only when a HEAD moved.
//! 5. `gate pr --branch` only: `GET /repos/{o}/{r}/pulls/{n}` for linked open or
//!    merged PRs authored by the login, at most [`MAX_PR_LOOKUPS`], to read
//!    their head branch.
//!
//! Every read except (1) and the GraphQL queries is conditional; a 304 is a live confirmation. Network
//! or quota failure ⇒ `Err` (the CLI exits 1); the attempt is still written to
//! `gate_log` with verdict `error`. Nothing falls back to the cache.

use crate::claims::ClaimService;
use crate::config::Config;
use crate::coordinator::Coordinator;
use crate::error::{Result, ScoutError};
use crate::freshness::{Freshness, Live, LiveRead};
use crate::gateway::Gateway;
use crate::index::{GitHubIndexer, IssueIndexer};
use crate::issue_marks::{IssueMarkStatus, IssueVerdict};
use crate::models::{
    normalize_label, AgentId, BlockReason, Claim, ClaimOutcome, GateFinding, GateKind,
    GateObservation, GateRaw, GateReport, GateVerdict, IssueDetail, IssueRef, IssueState, LinkedPr,
    ObservationKind, ObservationSeverity, PolicyFile, PolicyMark, RepoName, RepoProfile,
    Requirement, Restriction, Stance,
};
use crate::policy::files::graphql_data;
use crate::policy::{GitHubPolicyFiles, PolicyFiles};
use crate::profile::{GitHubProfiler, RepoProfiler};
use crate::store::{SharedStore, Store};
use crate::transport::ApiRequest;
use anyhow::Context;
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use std::sync::{Arc, MutexGuard, PoisonError};

/// Upper bound on `pulls/{n}` lookups per `gate pr` (own-branch detection).
pub const MAX_PR_LOOKUPS: usize = 5;

/// Comment phrases reported as a possible claim on the work. Heuristic only:
/// matches are `info` observations, never blocking (§8).
pub const CLAIM_PHRASES: &[&str] = &[
    "i'll take",
    "i will take",
    "i'd like to take",
    "i would like to take",
    "i'd like to work on",
    "i would like to work on",
    "i'll work on",
    "i will work on",
    "i'm working on",
    "i am working on",
    "working on this",
    "can i work on",
    "could i work on",
    "may i work on",
    "can i take",
    "assign me",
    "assign this to me",
    "assign it to me",
    "/assign",
    "i'll pick this",
    "i'm on it",
    "let me take",
];

/// Author associations treated as maintainers when listing approval evidence.
const MAINTAINER_ASSOCIATIONS: &[&str] = &["OWNER", "MEMBER", "COLLABORATOR"];

/// `gate pr` options.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PrOptions {
    /// The agent's own PR head branch, excluded from `pr_exists`.
    pub branch: Option<String>,
    /// Where the maintainer approved the work, when the mark requires approval.
    pub approval_url: Option<String>,
}

pub trait Gatekeeper {
    /// Live checks before starting work; acquires or renews the local claim on pass.
    fn start(&self, issue: &IssueRef, agent: &AgentId) -> Result<GateReport>;

    /// Live checks before opening a PR; the claim must be held.
    fn pr(&self, issue: &IssueRef, agent: &AgentId, options: &PrOptions) -> Result<GateReport>;
}

/// The head of a linked PR, read live for own-branch exclusion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullHead {
    /// The PR's `html_url`, as in [`LinkedPr::url`].
    pub url: String,
    pub head_ref: Option<String>,
    pub author: Option<String>,
}

/// Everything a gate decides from: live GitHub data plus local mark and claim state.
pub struct GateInputs {
    /// The authenticated GitHub login.
    pub viewer: Live<String>,
    pub issue: Live<IssueDetail>,
    pub repo: Live<RepoProfile>,
    pub policy_files: Live<Vec<PolicyFile>>,
    /// `gate pr --branch`: heads of the login's linked PRs.
    pub pull_heads: Vec<Live<PullHead>>,
    /// Local, agent-written (§5); not GitHub data.
    pub mark: Option<PolicyMark>,
    /// Local, agent-written (§5b).
    pub issue_mark: Option<IssueMarkStatus>,
    /// The active claim on the issue, whoever holds it (before any acquire).
    pub claim: Option<Claim>,
}

/// `pass` with no findings, else `blocked`.
pub fn verdict_for(findings: &[GateFinding]) -> GateVerdict {
    if findings.is_empty() {
        GateVerdict::Pass
    } else {
        GateVerdict::Blocked
    }
}

/// Whether a comment reads like someone taking the issue ([`CLAIM_PHRASES`]).
pub fn looks_like_claim(body: &str) -> bool {
    let text = body.to_lowercase().replace('\u{2019}', "'");
    CLAIM_PHRASES.iter().any(|phrase| text.contains(phrase))
}

/// Whether the mark requires maintainer approval before a PR.
pub fn approval_required(mark: &PolicyMark) -> bool {
    mark.scope.needs_prior_approval
        || mark
            .requirements
            .contains(&Requirement::MaintainerApprovalOnIssue)
}

/// One checklist line for a requirement.
pub fn requirement_text(requirement: &Requirement) -> String {
    match requirement {
        Requirement::DiscloseInPr => "Disclose AI assistance in the PR description".into(),
        Requirement::AssistedByTrailer => "Add an `Assisted-by:` trailer to every commit".into(),
        Requirement::HumanWrittenDescription => {
            "The PR description must be written by a human".into()
        }
        Requirement::HumanWrittenReplies => {
            "Replies to reviewers must be written by a human".into()
        }
        Requirement::MaintainerApprovalOnIssue => {
            "Get maintainer approval on the issue before opening the PR".into()
        }
        Requirement::LabelPr(label) => format!("Apply the label `{label}` to the PR"),
        Requirement::Unknown(value) => format!("Requirement {value}"),
    }
}

fn is_pr_template(path: &str) -> bool {
    path.to_ascii_lowercase().contains("pull_request_template")
}

fn one_line(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        flat
    } else {
        let cut: String = flat.chars().take(max).collect();
        format!("{cut}…")
    }
}

fn same_login(a: Option<&str>, viewer: &str) -> bool {
    a.is_some_and(|a| a.eq_ignore_ascii_case(viewer))
}

/// Closed-unmerged PRs do not block; open, merged, or unknown state does (fail closed).
fn pr_blocks(pr: &LinkedPr) -> bool {
    pr.state.as_deref() != Some("closed")
}

fn observe(
    kind: ObservationKind,
    severity: ObservationSeverity,
    detail: String,
    url: Option<String>,
) -> GateObservation {
    GateObservation {
        kind,
        severity,
        detail,
        url,
    }
}

fn claimed_by_other(claim: &Claim) -> GateFinding {
    GateFinding {
        reason: BlockReason::ClaimedByOther,
        detail: format!(
            "claimed locally by {} until {}",
            claim.agent,
            claim.lease_until.to_rfc3339()
        ),
    }
}

/// Pure decision over live inputs; no I/O. The claim is not acquired here.
pub fn evaluate(
    kind: GateKind,
    issue: &IssueRef,
    agent: &AgentId,
    options: &PrOptions,
    inputs: GateInputs,
    now: DateTime<Utc>,
) -> GateReport {
    let mut as_of = [
        inputs.viewer.as_of(),
        inputs.issue.as_of(),
        inputs.repo.as_of(),
        inputs.policy_files.as_of(),
    ]
    .into_iter()
    .min()
    .unwrap_or(now);
    for head in &inputs.pull_heads {
        as_of = as_of.min(head.as_of());
    }
    let viewer = inputs.viewer.into_inner();
    let heads: Vec<PullHead> = inputs
        .pull_heads
        .into_iter()
        .map(Live::into_inner)
        .collect();
    let mut detail = inputs.issue.into_inner();
    for pr in &mut detail.linked_prs {
        if let Some(head) = heads.iter().find(|h| h.url == pr.url) {
            pr.head_ref.clone_from(&head.head_ref);
        }
    }
    let repo = inputs.repo.into_inner();
    let files = inputs.policy_files.into_inner();
    let mark = inputs.mark;

    let mut findings = Vec::new();
    let mut observations = Vec::new();
    let mut block = |reason: BlockReason, detail: String| {
        findings.push(GateFinding { reason, detail });
    };
    let row = &detail.row;

    if row.state != IssueState::Open {
        block(
            BlockReason::IssueClosed,
            format!("{issue} is {}", row.state),
        );
    }

    if let Some(status) = &inputs.issue_mark {
        let mark = &status.mark;
        let reason = mark
            .reason
            .as_ref()
            .map(|r| format!(" ({r})"))
            .unwrap_or_default();
        let summary = format!(
            "marked {}{reason} by {} on {}: \"{}\"",
            mark.verdict,
            mark.marked_by,
            mark.marked_at.format("%Y-%m-%d"),
            mark.note
        );
        if mark.verdict == IssueVerdict::Skip && status.active {
            block(
                BlockReason::IssueSkipped,
                format!(
                    "{summary}; run `contrib-scout issue unmark {issue}` only if the verdict is wrong"
                ),
            );
        } else {
            let expired = if status.active { "" } else { " (expired)" };
            observations.push(observe(
                ObservationKind::IssueMark,
                ObservationSeverity::Info,
                format!("{summary}{expired}"),
                None,
            ));
        }
    }

    let others: Vec<&str> = row
        .assignees
        .iter()
        .map(String::as_str)
        .filter(|a| !a.eq_ignore_ascii_case(&viewer))
        .collect();
    if !others.is_empty() {
        block(
            BlockReason::AssignedToOther,
            format!("assigned to {}", others.join(", ")),
        );
    } else if !row.assignees.is_empty() {
        observations.push(observe(
            ObservationKind::AssignedToYou,
            ObservationSeverity::Info,
            format!("assigned to you ({viewer})"),
            None,
        ));
    }

    for pr in &detail.linked_prs {
        let state = pr.state.as_deref().unwrap_or("unknown");
        let by = pr
            .author
            .as_deref()
            .map(|a| format!(", by {a}"))
            .unwrap_or_default();
        if !pr_blocks(pr) {
            observations.push(observe(
                ObservationKind::ClosedPr,
                ObservationSeverity::Info,
                format!("linked PR closed without merging{by}"),
                Some(pr.url.clone()),
            ));
            continue;
        }
        let own = kind == GateKind::Pr
            && options.branch.is_some()
            && pr.head_ref.is_some()
            && pr.head_ref == options.branch
            && same_login(pr.author.as_deref(), &viewer);
        if own {
            observations.push(observe(
                ObservationKind::OwnPrExcluded,
                ObservationSeverity::Info,
                format!(
                    "your own PR from branch {} is excluded",
                    pr.head_ref.as_deref().unwrap_or_default()
                ),
                Some(pr.url.clone()),
            ));
            continue;
        }
        block(
            BlockReason::PrExists,
            format!("{} ({state}{by}) is linked via {}", pr.url, pr.event),
        );
    }

    for comment in detail.comments.iter().filter(|c| looks_like_claim(&c.body)) {
        observations.push(observe(
            ObservationKind::CommentClaim,
            ObservationSeverity::Info,
            format!(
                "{} on {}: \"{}\"",
                comment.author.as_deref().unwrap_or("unknown"),
                comment.created_at.format("%Y-%m-%d"),
                one_line(&comment.body, 160)
            ),
            Some(comment.url.clone()),
        ));
    }

    if repo.archived {
        block(
            BlockReason::RepoArchived,
            format!("{} is archived", repo.repo),
        );
    }

    match &mark {
        None => block(
            BlockReason::PolicyUnmarked,
            format!(
                "no policy mark for {repo} or its org; run `contrib-scout policy files {repo}`, read the text, then record it with `contrib-scout policy set`",
                repo = issue.repo
            ),
        ),
        Some(mark) => {
            if mark.stance == Stance::Bans {
                block(
                    BlockReason::PolicyBans,
                    format!("{} is marked bans: \"{}\"", mark.target, mark.note),
                );
            }
            if mark.review_due(now) {
                observations.push(observe(
                    ObservationKind::MarkReviewDue,
                    ObservationSeverity::Warning,
                    format!(
                        "the mark on {} expired {}; re-read the policy files and re-mark",
                        mark.target,
                        mark.expires_at.format("%Y-%m-%d")
                    ),
                    None,
                ));
            }
            for label in &row.labels {
                let label_key = normalize_label(label);
                if mark
                    .scope
                    .forbidden_labels
                    .iter()
                    .any(|f| normalize_label(f) == label_key)
                {
                    block(
                        BlockReason::ForbiddenLabel,
                        format!(
                            "label `{label}` is forbidden for AI work by the mark on {}",
                            mark.target
                        ),
                    );
                }
            }
            if approval_required(mark) {
                match (kind, &options.approval_url) {
                    (GateKind::Start, _) => observations.push(observe(
                        ObservationKind::ApprovalRequired,
                        ObservationSeverity::Warning,
                        "the mark requires maintainer approval on the issue before a PR; `gate pr` blocks without --approval-url".into(),
                        None,
                    )),
                    (GateKind::Pr, None) => block(
                        BlockReason::ApprovalMissing,
                        "the mark requires maintainer approval; pass --approval-url URL pointing to it".into(),
                    ),
                    (GateKind::Pr, Some(url)) => observations.push(observe(
                        ObservationKind::ApprovalUrl,
                        ObservationSeverity::Info,
                        "approval supplied by the agent; not verified by the CLI".into(),
                        Some(url.clone()),
                    )),
                }
                for comment in detail.comments.iter().filter(|c| {
                    c.author_association
                        .as_deref()
                        .is_some_and(|a| MAINTAINER_ASSOCIATIONS.contains(&a))
                }) {
                    observations.push(observe(
                        ObservationKind::ApprovalEvidence,
                        ObservationSeverity::Info,
                        format!(
                            "maintainer comment by {} ({}) on {}; read it for approval: \"{}\"",
                            comment.author.as_deref().unwrap_or("unknown"),
                            comment.author_association.as_deref().unwrap_or_default(),
                            comment.created_at.format("%Y-%m-%d"),
                            one_line(&comment.body, 120)
                        ),
                        Some(comment.url.clone()),
                    ));
                }
            }
            // `code: forbidden` means an AI agent cannot do the work at all, so
            // it blocks `gate start` too (a `bans` stance already blocks).
            let code_forbidden =
                mark.scope.code == Restriction::Forbidden && mark.stance != Stance::Bans;
            let why: Vec<&str> = match kind {
                GateKind::Pr => mark
                    .human_required_reasons()
                    .into_iter()
                    .filter(|r| *r != "code: forbidden" || code_forbidden)
                    .collect(),
                GateKind::Start if code_forbidden => vec!["code: forbidden"],
                GateKind::Start => Vec::new(),
            };
            if !why.is_empty() {
                block(
                    BlockReason::HumanRequired,
                    format!(
                        "the mark on {} says {}; stop and hand the work to a human",
                        mark.target,
                        why.join(", ")
                    ),
                );
            }
        }
    }

    match (&inputs.claim, kind) {
        (Some(claim), _) if &claim.agent != agent => {
            let finding = claimed_by_other(claim);
            block(finding.reason, finding.detail);
        }
        (None, GateKind::Pr) => block(
            BlockReason::ClaimNotHeld,
            format!("you hold no active claim; run `contrib-scout gate start {issue}` first"),
        ),
        _ => {}
    }

    if kind == GateKind::Pr {
        for file in files.iter().filter(|f| f.truncated) {
            observations.push(observe(
                ObservationKind::PolicyFileTruncated,
                ObservationSeverity::Warning,
                format!(
                    "{}/{} is truncated ({} bytes in full); read the rest at the URL",
                    file.repo, file.path, file.size
                ),
                Some(file.url.clone()),
            ));
        }
    }

    let requirements = mark
        .as_ref()
        .map(|m| m.requirements.clone())
        .unwrap_or_default();
    let checklist = if kind == GateKind::Pr {
        checklist(mark.as_ref(), options, &files)
    } else {
        Vec::new()
    };
    let verdict = verdict_for(&findings);
    GateReport {
        kind,
        issue: issue.clone(),
        agent: agent.clone(),
        verdict,
        blocked_by: findings,
        observations,
        requirements,
        checklist,
        claim: inputs.claim.filter(|c| &c.agent == agent),
        viewer: Some(viewer),
        approval_url: options.approval_url.clone(),
        error: None,
        raw: GateRaw {
            issue: Some(detail),
            repo: Some(repo),
            policy_mark: mark,
            issue_mark: inputs.issue_mark.map(|s| s.mark),
            policy_files: files,
        },
        as_of,
    }
}

/// The full `gate pr` checklist: requirements, restricted scope, approval,
/// and every policy file to read (PR templates to follow).
fn checklist(mark: Option<&PolicyMark>, options: &PrOptions, files: &[PolicyFile]) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(mark) = mark {
        lines.extend(mark.requirements.iter().map(requirement_text));
        let scope = &mark.scope;
        for (name, restriction) in [
            ("code", &scope.code),
            ("pr_text", &scope.pr_text),
            ("comments", &scope.comments),
            ("autonomous_agents", &scope.autonomous_agents),
        ] {
            if !matches!(restriction, Restriction::Unspecified | Restriction::Allowed) {
                lines.push(format!("Scope {name}: {restriction}"));
            }
        }
        if !scope.forbidden_labels.is_empty() {
            lines.push(format!(
                "No AI work on issues labelled: {}",
                scope.forbidden_labels.join(", ")
            ));
        }
        if approval_required(mark) {
            lines.push(match &options.approval_url {
                Some(url) => format!("Maintainer approval: {url}"),
                None => "Maintainer approval: missing".into(),
            });
        }
    }
    for file in files {
        if is_pr_template(&file.path) {
            lines.push(format!(
                "Follow the PR template {}/{}: {}",
                file.repo, file.path, file.url
            ));
        } else {
            lines.push(format!("Read {}/{}: {}", file.repo, file.path, file.url));
        }
    }
    lines
}

/// `/repos/{o}/{r}/pulls/{n}` for a PR `html_url` on github.com.
pub fn pull_api_path(html_url: &str) -> Option<String> {
    let rest = html_url.strip_prefix("https://github.com/")?;
    let mut parts = rest.split('/');
    let repo = RepoName::new(parts.next()?, parts.next()?).ok()?;
    if parts.next()? != "pull" {
        return None;
    }
    let number: u64 = parts.next()?.parse().ok()?;
    Some(format!(
        "/repos/{}/{}/pulls/{number}",
        repo.owner(),
        repo.name()
    ))
}

/// Linked PRs of an issue as GraphQL closing references (§8).
pub const CLOSING_PRS_QUERY: &str = "query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    issue(number: $number) {
      closedByPullRequestsReferences(first: 20, includeClosedPrs: true) {
        nodes { number url state author { login } headRefName merged }
      }
    }
  }
}
";

/// `event` of a [`LinkedPr`] found through [`CLOSING_PRS_QUERY`].
pub const CLOSING_REFERENCE_EVENT: &str = "closing_reference";

/// [`LinkedPr`]s from `closedByPullRequestsReferences.nodes`.
pub fn parse_closing_prs(nodes: &[Value]) -> Vec<LinkedPr> {
    nodes
        .iter()
        .filter_map(|node| {
            let url = node["url"].as_str()?.to_owned();
            let state = if node["merged"].as_bool() == Some(true) {
                Some("merged".to_owned())
            } else {
                node["state"].as_str().map(str::to_ascii_lowercase)
            };
            Some(LinkedPr {
                url,
                event: CLOSING_REFERENCE_EVENT.into(),
                author: node["author"]["login"].as_str().map(str::to_owned),
                state,
                head_ref: node["headRefName"].as_str().map(str::to_owned),
            })
        })
        .collect()
}

/// Adds closing-reference PRs to the timeline's linked PRs: new URLs are
/// appended, known ones get missing author / state / head branch filled in.
fn merge_linked_prs(detail: Live<IssueDetail>, closing: Live<Vec<LinkedPr>>) -> Live<IssueDetail> {
    let as_of = detail.as_of().min(closing.as_of());
    let mut detail = detail.into_inner();
    for pr in closing.into_inner() {
        match detail
            .linked_prs
            .iter_mut()
            .find(|known| known.url == pr.url)
        {
            Some(known) => {
                if known.author.is_none() {
                    known.author = pr.author;
                }
                if known.state.is_none() {
                    known.state = pr.state;
                }
                if known.head_ref.is_none() {
                    known.head_ref = pr.head_ref;
                }
            }
            None => detail.linked_prs.push(pr),
        }
    }
    Live::new(detail, as_of, false)
}

/// The default gatekeeper over the live service methods and one [`Gateway`].
pub struct LiveGatekeeper {
    gateway: Arc<Gateway>,
    store: SharedStore,
    policy_files: GitHubPolicyFiles,
    profiler: GitHubProfiler,
    indexer: GitHubIndexer,
    claims: ClaimService,
}

impl LiveGatekeeper {
    pub fn new(
        gateway: Arc<Gateway>,
        store: SharedStore,
        config: Config,
        coordinator: Arc<dyn Coordinator>,
    ) -> Self {
        Self {
            policy_files: GitHubPolicyFiles::new(gateway.clone(), store.clone(), config.clone()),
            profiler: GitHubProfiler::new(gateway.clone(), store.clone(), config.clone()),
            indexer: GitHubIndexer::new(gateway.clone(), store.clone(), config.clone()),
            claims: ClaimService::new(coordinator, store.clone(), config.claim_lease()),
            gateway,
            store,
        }
    }

    pub fn claims(&self) -> &ClaimService {
        &self.claims
    }

    fn store(&self) -> MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The authenticated login: one uncached `GET /user`.
    fn viewer(&self, _live: LiveRead) -> Result<Live<String>> {
        let response = self.gateway.send(&ApiRequest::get("/user"))?;
        if response.status != 200 {
            return Err(ScoutError::HttpStatus {
                status: response.status,
                url: "/user".into(),
            }
            .into());
        }
        let body: Value = response.json_body().context("parse GET /user")?;
        let login = body["login"]
            .as_str()
            .filter(|login| !login.is_empty())
            .context("GET /user returned no login")?;
        Ok(Live::new(login.to_owned(), Utc::now(), false))
    }

    /// The head branch of one linked PR; `None` when the URL is not a
    /// github.com PR or the PR is gone (it then keeps blocking).
    fn pull_head(&self, pr: &LinkedPr, live: LiveRead) -> Result<Option<Live<PullHead>>> {
        let Some(path) = pull_api_path(&pr.url) else {
            return Ok(None);
        };
        let response = self.gateway.get_live(&ApiRequest::get(path), live)?;
        if response.get().status == 404 {
            return Ok(None);
        }
        let body: Value = response
            .get()
            .json_body()
            .with_context(|| format!("parse pull request {}", pr.url))?;
        let head = PullHead {
            url: pr.url.clone(),
            head_ref: body["head"]["ref"].as_str().map(str::to_owned),
            author: body["user"]["login"].as_str().map(str::to_owned),
        };
        Ok(Some(response.map(|_| head)))
    }

    /// PRs linked to the issue as closing references (Development sidebar
    /// links and closing keywords), open or closed: one GraphQL query.
    /// Sidebar links may leave no usable timeline event, so gates ask for
    /// them directly. GraphQL has no ETags; this is always a network read.
    fn closing_prs(&self, issue: &IssueRef) -> Result<Live<Vec<LinkedPr>>> {
        let variables = json!({
            "owner": issue.repo.owner(),
            "name": issue.repo.name(),
            "number": issue.number,
        });
        let response = self.gateway.graphql(CLOSING_PRS_QUERY, &variables)?;
        let at = Utc::now();
        let data =
            graphql_data(&response).with_context(|| format!("closing pull requests of {issue}"))?;
        let nodes = data
            .pointer("/repository/issue/closedByPullRequestsReferences/nodes")
            .and_then(Value::as_array)
            .with_context(|| format!("GraphQL returned no closing references for {issue}"))?;
        Ok(Live::new(parse_closing_prs(nodes), at, false))
    }

    fn gather(
        &self,
        kind: GateKind,
        issue: &IssueRef,
        options: &PrOptions,
        now: DateTime<Utc>,
    ) -> Result<GateInputs> {
        let live = Freshness::Live.require_live("gate")?;
        let viewer = self.viewer(live)?;
        let repo = self.profiler.profile_live(&issue.repo, live)?;
        let detail = self.indexer.issue_live(issue, live)?;
        if detail.get().is_pull_request {
            return Err(ScoutError::Usage(format!(
                "{issue} is a pull request, not an issue; gates take the issue the PR is for"
            ))
            .into());
        }
        let closing = self.closing_prs(issue)?;
        let detail = merge_linked_prs(detail, closing);
        let policy_files = self
            .policy_files
            .fetch_live(&issue.repo, live, kind == GateKind::Pr)?;
        let mut pull_heads = Vec::new();
        if kind == GateKind::Pr && options.branch.is_some() {
            let own = detail
                .get()
                .linked_prs
                .iter()
                .filter(|pr| pr_blocks(pr) && same_login(pr.author.as_deref(), viewer.get()))
                .take(MAX_PR_LOOKUPS);
            for pr in own {
                pull_heads.extend(self.pull_head(pr, live)?);
            }
        }
        let (mark, issue_mark) = {
            let store = self.store();
            (
                store.effective_mark(&issue.repo)?,
                store.issue_mark_status(issue, now)?,
            )
        };
        let claim = self.claims.active(issue)?;
        Ok(GateInputs {
            viewer,
            issue: detail,
            repo,
            policy_files,
            pull_heads,
            mark,
            issue_mark,
            claim,
        })
    }

    /// Acquires the claim on a passing `start`; records the branch on a passing `pr`.
    fn settle_claim(
        &self,
        report: &mut GateReport,
        issue: &IssueRef,
        agent: &AgentId,
        options: &PrOptions,
    ) -> Result<()> {
        if report.verdict != GateVerdict::Pass {
            return Ok(());
        }
        match report.kind {
            GateKind::Start => match self.claims.acquire(issue, agent)? {
                ClaimOutcome::Acquired { claim } | ClaimOutcome::Renewed { claim } => {
                    report.claim = Some(claim);
                }
                ClaimOutcome::HeldByOther { claim } => {
                    report.blocked_by.push(claimed_by_other(&claim));
                    report.verdict = GateVerdict::Blocked;
                }
            },
            GateKind::Pr => {
                if let Some(branch) = &options.branch {
                    report.claim = Some(self.claims.record_branch(issue, agent, branch)?);
                }
            }
        }
        Ok(())
    }

    fn run(
        &self,
        kind: GateKind,
        issue: &IssueRef,
        agent: &AgentId,
        options: &PrOptions,
    ) -> Result<GateReport> {
        let now = Utc::now();
        let result = self.gather(kind, issue, options, now).and_then(|inputs| {
            let mut report = evaluate(kind, issue, agent, options, inputs, now);
            self.settle_claim(&mut report, issue, agent, options)?;
            Ok(report)
        });
        match result {
            Ok(report) => {
                self.store().log_gate(&report)?;
                Ok(report)
            }
            // A wrong ref (e.g. a pull request) is not a gate run: exit 2, no log.
            Err(error) if matches!(ScoutError::find(&error), Some(ScoutError::Usage(_))) => {
                Err(error)
            }
            Err(error) => {
                let failed = GateReport {
                    kind,
                    issue: issue.clone(),
                    agent: agent.clone(),
                    verdict: GateVerdict::Error,
                    blocked_by: Vec::new(),
                    observations: Vec::new(),
                    requirements: Vec::new(),
                    checklist: Vec::new(),
                    claim: None,
                    viewer: None,
                    approval_url: options.approval_url.clone(),
                    error: Some(format!("{error:#}")),
                    raw: GateRaw::default(),
                    as_of: now,
                };
                // Best effort: the gate already failed closed.
                let _ = self.store().log_gate(&failed);
                Err(error.context(format!(
                    "gate {} {issue} failed closed (verdict: error)",
                    kind.as_str()
                )))
            }
        }
    }
}

impl Gatekeeper for LiveGatekeeper {
    fn start(&self, issue: &IssueRef, agent: &AgentId) -> Result<GateReport> {
        self.run(GateKind::Start, issue, agent, &PrOptions::default())
    }

    fn pr(&self, issue: &IssueRef, agent: &AgentId, options: &PrOptions) -> Result<GateReport> {
        self.run(GateKind::Pr, issue, agent, options)
    }
}
