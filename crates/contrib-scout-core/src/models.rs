//! Shared data model (§4, §5, §6, §8).
//!
//! Every type serializes with snake_case names. Enums that are read back from
//! the database or from GitHub carry an `Unknown(String)` fallback so a newer
//! writer never breaks an older reader; their `FromStr` (used for CLI input)
//! stays strict and rejects unknown values.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::str::FromStr;

/// Why a value could not be parsed. Rendered by clap as a usage error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError {
    pub what: &'static str,
    pub input: String,
    pub reason: String,
}

impl ParseError {
    fn new(what: &'static str, input: &str, reason: impl Into<String>) -> Self {
        Self {
            what,
            input: input.to_owned(),
            reason: reason.into(),
        }
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid {} {:?}: {}", self.what, self.input, self.reason)
    }
}

impl std::error::Error for ParseError {}

/// Defines a string-valued enum with a forward-compatible `Unknown(String)`
/// variant: strict `FromStr` (accepts `-` for `_`, any case), lenient serde.
macro_rules! open_enum {
    ($(#[$meta:meta])* $name:ident, $what:literal { $($(#[$vmeta:meta])* $variant:ident => $text:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Clone, Debug, PartialEq, Eq, Hash)]
        pub enum $name {
            $($(#[$vmeta])* $variant,)+
            /// A value written by a newer version; preserved verbatim.
            Unknown(String),
        }

        impl $name {
            /// Every known value, in declaration order.
            pub const KNOWN: &'static [&'static str] = &[$($text),+];

            pub fn as_str(&self) -> &str {
                match self {
                    $(Self::$variant => $text,)+
                    Self::Unknown(other) => other,
                }
            }

            /// Lenient conversion used when reading stored or remote data.
            pub fn from_stored(value: &str) -> Self {
                match value {
                    $($text => Self::$variant,)+
                    other => Self::Unknown(other.to_owned()),
                }
            }
        }

        impl FromStr for $name {
            type Err = ParseError;

            fn from_str(input: &str) -> Result<Self, Self::Err> {
                let normalized = input.trim().to_ascii_lowercase().replace('-', "_");
                match Self::from_stored(&normalized) {
                    Self::Unknown(_) => Err(ParseError::new(
                        $what,
                        input,
                        format!("expected one of {}", Self::KNOWN.join(", ")),
                    )),
                    known => Ok(known),
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(self.as_str())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let value = String::deserialize(deserializer)?;
                Ok(Self::from_stored(&value))
            }
        }
    };
}

/// Implements `TryFrom<String>`/`Into<String>`/`Display` for a validated string newtype.
macro_rules! string_newtype {
    ($name:ident) => {
        impl TryFrom<String> for $name {
            type Error = ParseError;
            fn try_from(value: String) -> Result<Self, Self::Error> {
                value.parse()
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> String {
                value.to_string()
            }
        }
    };
}

// ---------------------------------------------------------------------------
// Names and references
// ---------------------------------------------------------------------------

fn validate_owner(input: &str, owner: &str) -> Result<(), ParseError> {
    let valid = !owner.is_empty()
        && owner.len() <= 39
        && !owner.starts_with('-')
        && owner
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-');
    if valid {
        Ok(())
    } else {
        Err(ParseError::new(
            "owner",
            input,
            "owners are 1-39 letters, digits, or hyphens and cannot start with a hyphen",
        ))
    }
}

fn validate_repo(input: &str, name: &str) -> Result<(), ParseError> {
    let valid = !name.is_empty()
        && name.len() <= 100
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if valid {
        Ok(())
    } else {
        Err(ParseError::new(
            "repository",
            input,
            "repository names are 1-100 letters, digits, '.', '_', or '-'",
        ))
    }
}

/// Strips an optional `https://github.com/` (or `github.com/`) prefix.
fn strip_github_prefix(input: &str) -> &str {
    let rest = input
        .strip_prefix("https://")
        .or_else(|| input.strip_prefix("http://"))
        .unwrap_or(input);
    rest.strip_prefix("github.com/")
        .or_else(|| rest.strip_prefix("www.github.com/"))
        .unwrap_or(input)
}

/// A GitHub repository, `owner/name`. GitHub names are case-insensitive; the
/// store compares them with `COLLATE NOCASE` and this type keeps the spelling given.
///
/// Accepts `owner/name`, `https://github.com/owner/name`, with an optional
/// trailing `/` or `.git`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RepoName {
    owner: String,
    name: String,
}

impl RepoName {
    pub fn new(owner: &str, name: &str) -> Result<Self, ParseError> {
        format!("{owner}/{name}").parse()
    }

    pub fn owner(&self) -> &str {
        &self.owner
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// The organization (or user) that owns this repository.
    pub fn org(&self) -> OrgName {
        OrgName(self.owner.clone())
    }

    /// The `{owner}/.github` repository that holds organization-wide policy (§2).
    pub fn org_policy_repo(&self) -> RepoName {
        RepoName {
            owner: self.owner.clone(),
            name: ".github".into(),
        }
    }

    /// Lowercased `owner/name`, for map keys and lock names.
    pub fn key(&self) -> String {
        self.to_string().to_ascii_lowercase()
    }
}

impl FromStr for RepoName {
    type Err = ParseError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let trimmed = input.trim();
        let rest = strip_github_prefix(trimmed).trim_end_matches('/');
        let rest = rest.strip_suffix(".git").unwrap_or(rest);
        let Some((owner, name)) = rest.split_once('/') else {
            return Err(ParseError::new("repository", input, "expected owner/name"));
        };
        if name.contains('/') {
            return Err(ParseError::new("repository", input, "expected owner/name"));
        }
        validate_owner(input, owner)?;
        validate_repo(input, name)?;
        Ok(Self {
            owner: owner.to_owned(),
            name: name.to_owned(),
        })
    }
}

impl fmt::Display for RepoName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.owner, self.name)
    }
}

string_newtype!(RepoName);

/// A GitHub organization or user login.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct OrgName(String);

impl OrgName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for OrgName {
    type Err = ParseError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let owner = strip_github_prefix(input.trim()).trim_end_matches('/');
        validate_owner(input, owner)?;
        Ok(Self(owner.to_owned()))
    }
}

impl fmt::Display for OrgName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

string_newtype!(OrgName);

/// An issue, `owner/repo#N`. Also parses `https://github.com/owner/repo/issues/N`
/// (query strings and fragments are ignored).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct IssueRef {
    pub repo: RepoName,
    pub number: u64,
}

impl IssueRef {
    pub fn new(repo: RepoName, number: u64) -> Self {
        Self { repo, number }
    }

    /// The issue's web URL.
    pub fn html_url(&self) -> String {
        format!("https://github.com/{}/issues/{}", self.repo, self.number)
    }
}

fn parse_issue_number(input: &str, value: &str) -> Result<u64, ParseError> {
    match value.parse::<u64>() {
        Ok(number) if number > 0 && value.bytes().all(|b| b.is_ascii_digit()) => Ok(number),
        _ => Err(ParseError::new(
            "issue reference",
            input,
            "the issue number must be a positive integer",
        )),
    }
}

impl FromStr for IssueRef {
    type Err = ParseError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let trimmed = input.trim();
        if let Some((repo, number)) = trimmed.rsplit_once('#') {
            if !repo.contains("/issues/") {
                return Ok(Self {
                    repo: repo.parse()?,
                    number: parse_issue_number(input, number)?,
                });
            }
        }
        let path = strip_github_prefix(trimmed);
        let path = path.split(['?', '#']).next().unwrap_or(path);
        let path = path.trim_end_matches('/');
        let mut parts = path.split('/');
        match (
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
        ) {
            (Some(owner), Some(name), Some("issues"), Some(number), None) => Ok(Self {
                repo: RepoName::new(owner, name)
                    .map_err(|e| ParseError::new("issue reference", input, e.reason))?,
                number: parse_issue_number(input, number)?,
            }),
            _ => Err(ParseError::new(
                "issue reference",
                input,
                "expected owner/repo#N or https://github.com/owner/repo/issues/N",
            )),
        }
    }
}

impl fmt::Display for IssueRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}#{}", self.repo, self.number)
    }
}

string_newtype!(IssueRef);

/// Identifies one agent for claims, history, and request accounting (§9).
/// From `--agent`, `CONTRIB_SCOUT_AGENT`, or a session id.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct AgentId(String);

impl AgentId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for AgentId {
    type Err = ParseError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let value = input.trim();
        if value.is_empty() || value.len() > 128 {
            return Err(ParseError::new(
                "agent id",
                input,
                "must be 1-128 characters",
            ));
        }
        if value.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(ParseError::new(
                "agent id",
                input,
                "must not contain whitespace or control characters",
            ));
        }
        Ok(Self(value.to_owned()))
    }
}

impl fmt::Display for AgentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

string_newtype!(AgentId);

// ---------------------------------------------------------------------------
// AI policy marks (§5)
// ---------------------------------------------------------------------------

open_enum! {
    /// The agent's reading of a project's AI policy. Chosen by the agent, never by the CLI.
    Stance, "stance" {
        Bans => "bans",
        Restricts => "restricts",
        Disclose => "disclose",
        Allows => "allows",
        Welcomes => "welcomes",
        NoneFound => "none_found",
    }
}

impl Stance {
    /// Short human label for reminders, e.g. `Bans AI`.
    pub fn label(&self) -> String {
        match self {
            Self::Bans => "Bans AI".into(),
            Self::Restricts => "Restricts AI".into(),
            Self::Disclose => "Requires AI disclosure".into(),
            Self::Allows => "Allows AI".into(),
            Self::Welcomes => "Welcomes AI".into(),
            Self::NoneFound => "No AI policy found".into(),
            Self::Unknown(other) => format!("Stance {other}"),
        }
    }
}

open_enum! {
    /// How strongly one part of the contribution is restricted.
    #[derive(Default)]
    Restriction, "restriction" {
        /// The policy says nothing about this part.
        #[default]
        Unspecified => "unspecified",
        Allowed => "allowed",
        /// Allowed if AI use is declared.
        Disclose => "disclose",
        /// Must be written by a human.
        HumanWritten => "human_written",
        Forbidden => "forbidden",
    }
}

/// Canonical form of a GitHub label for comparisons: lowercase (Unicode),
/// `-` and `_` read as spaces, surrounding space trimmed, inner runs of
/// whitespace collapsed. `Good-First_Issue ` and `good  first issue` match.
pub fn normalize_label(label: &str) -> String {
    label
        .to_lowercase()
        .replace(['-', '_'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Which parts of a contribution the policy restricts, filled in by the agent.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PolicyScope {
    pub code: Restriction,
    pub pr_text: Restriction,
    pub comments: Restriction,
    pub autonomous_agents: Restriction,
    /// Labels whose issues must not be worked with AI, e.g. `good first issue` (§2).
    pub forbidden_labels: Vec<String>,
    /// Maintainer approval on the issue is required before starting (e.g. clap).
    pub needs_prior_approval: bool,
}

impl PolicyScope {
    /// Keys accepted by [`PolicyScope::apply`] and `policy set --scope K=V`.
    pub const KEYS: &'static [&'static str] = &[
        "code",
        "pr_text",
        "comments",
        "autonomous_agents",
        "needs_prior_approval",
        "forbidden_label",
    ];

    /// `gate pr` blocks with `human_required` when this holds (§8): the PR
    /// text or the review replies must be written by a human (or must not
    /// come from AI at all), autonomous agents are forbidden, or the code
    /// itself must not be AI-written.
    pub fn requires_human(&self) -> bool {
        !self.human_required_reasons().is_empty()
    }

    /// The scope entries behind [`PolicyScope::requires_human`], as `key: value`.
    pub fn human_required_reasons(&self) -> Vec<&'static str> {
        let mut why = Vec::new();
        if self.code == Restriction::Forbidden {
            why.push("code: forbidden");
        }
        match self.pr_text {
            Restriction::HumanWritten => why.push("pr_text: human_written"),
            Restriction::Forbidden => why.push("pr_text: forbidden"),
            _ => {}
        }
        match self.comments {
            Restriction::HumanWritten => why.push("comments: human_written"),
            Restriction::Forbidden => why.push("comments: forbidden"),
            _ => {}
        }
        if self.autonomous_agents == Restriction::Forbidden {
            why.push("autonomous_agents: forbidden");
        }
        why
    }

    /// Applies one `K=V` setting. `forbidden_label` appends.
    pub fn apply(&mut self, setting: &ScopeSetting) -> Result<(), ParseError> {
        let ScopeSetting { key, value } = setting;
        let restriction = || value.parse::<Restriction>();
        match key.as_str() {
            "code" => self.code = restriction()?,
            "pr_text" => self.pr_text = restriction()?,
            "comments" => self.comments = restriction()?,
            "autonomous_agents" => self.autonomous_agents = restriction()?,
            "needs_prior_approval" => {
                self.needs_prior_approval = match value.to_ascii_lowercase().as_str() {
                    "true" | "yes" | "1" => true,
                    "false" | "no" | "0" => false,
                    _ => {
                        return Err(ParseError::new(
                            "scope",
                            &format!("{key}={value}"),
                            "expected true or false",
                        ))
                    }
                }
            }
            "forbidden_label" | "forbidden_labels" => {
                if !self.forbidden_labels.iter().any(|l| l == value) {
                    self.forbidden_labels.push(value.clone());
                }
            }
            _ => {
                return Err(ParseError::new(
                    "scope",
                    &format!("{key}={value}"),
                    format!("unknown key; expected one of {}", Self::KEYS.join(", ")),
                ))
            }
        }
        Ok(())
    }
}

/// One `--scope K=V` argument, validated on parse.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScopeSetting {
    pub key: String,
    pub value: String,
}

impl FromStr for ScopeSetting {
    type Err = ParseError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let Some((key, value)) = input.split_once('=') else {
            return Err(ParseError::new("scope", input, "expected KEY=VALUE"));
        };
        let setting = Self {
            key: key.trim().to_ascii_lowercase().replace('-', "_"),
            value: value.trim().to_owned(),
        };
        PolicyScope::default().apply(&setting)?;
        Ok(setting)
    }
}

/// Something a PR must do to satisfy the policy, listed by `gate pr` (§5, §8).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Requirement {
    DiscloseInPr,
    AssistedByTrailer,
    HumanWrittenDescription,
    HumanWrittenReplies,
    MaintainerApprovalOnIssue,
    /// Apply this label to the PR.
    LabelPr(String),
    /// A requirement written by a newer version; preserved verbatim.
    #[serde(untagged)]
    Unknown(serde_json::Value),
}

impl Requirement {
    pub const KNOWN: &'static [&'static str] = &[
        "disclose_in_pr",
        "assisted_by_trailer",
        "human_written_description",
        "human_written_replies",
        "maintainer_approval_on_issue",
        "label_pr=LABEL",
    ];
}

impl FromStr for Requirement {
    type Err = ParseError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let trimmed = input.trim();
        let (key, value) = match trimmed.split_once(['=', ':']) {
            Some((key, value)) => (key, Some(value.trim())),
            None => (trimmed, None),
        };
        let key = key.trim().to_ascii_lowercase().replace('-', "_");
        let requirement = match (key.as_str(), value) {
            ("disclose_in_pr", None) => Self::DiscloseInPr,
            ("assisted_by_trailer", None) => Self::AssistedByTrailer,
            ("human_written_description", None) => Self::HumanWrittenDescription,
            ("human_written_replies", None) => Self::HumanWrittenReplies,
            ("maintainer_approval_on_issue", None) => Self::MaintainerApprovalOnIssue,
            ("label_pr", Some(label)) if !label.is_empty() => Self::LabelPr(label.to_owned()),
            _ => {
                return Err(ParseError::new(
                    "requirement",
                    input,
                    format!("expected one of {}", Self::KNOWN.join(", ")),
                ))
            }
        };
        Ok(requirement)
    }
}

/// Inclusive 1-based line range, `10` or `10-20`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineRange {
    pub start: u32,
    pub end: u32,
}

impl FromStr for LineRange {
    type Err = ParseError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let (start, end) = input.split_once('-').unwrap_or((input, input));
        match (start.parse::<u32>(), end.parse::<u32>()) {
            (Ok(start), Ok(end)) if start >= 1 && start <= end => Ok(Self { start, end }),
            _ => Err(ParseError::new(
                "line range",
                input,
                "expected N or N-M with 1 <= N <= M",
            )),
        }
    }
}

impl fmt::Display for LineRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.start == self.end {
            write!(f, "{}", self.start)
        } else {
            write!(f, "{}-{}", self.start, self.end)
        }
    }
}

/// One `--evidence PATH[:LINES]` argument. `PATH` is a repository path
/// (e.g. `.github/CONTRIBUTING.md`) or an external URL (§2: Zig, Gentoo).
/// The suffix after the last `:` is read as lines only if it looks like `N` or `N-M`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceSpec {
    pub path: String,
    pub lines: Option<LineRange>,
}

impl FromStr for EvidenceSpec {
    type Err = ParseError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Err(ParseError::new("evidence", input, "expected PATH[:LINES]"));
        }
        if let Some((path, lines)) = trimmed.rsplit_once(':') {
            let looks_like_lines =
                !lines.is_empty() && lines.bytes().all(|b| b.is_ascii_digit() || b == b'-');
            if looks_like_lines && !path.is_empty() {
                return Ok(Self {
                    path: path.to_owned(),
                    lines: Some(lines.parse()?),
                });
            }
        }
        Ok(Self {
            path: trimmed.to_owned(),
            lines: None,
        })
    }
}

/// Evidence for a mark: where the policy text is and what it says.
/// For `NoneFound`, records the paths that were checked.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<LineRange>,
    /// Permanent link (commit-pinned where possible).
    pub url: String,
    /// Original text quoted by the agent, at most [`Evidence::MAX_EXCERPT_LINES`] lines.
    #[serde(default)]
    pub excerpt: String,
}

impl Evidence {
    pub const MAX_EXCERPT_LINES: usize = 3;
}

/// What a mark applies to. An org mark covers every repository in the org;
/// a repository mark takes precedence over its org's mark (§5, §13).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "name", rename_all = "snake_case")]
pub enum MarkTarget {
    Repo(RepoName),
    Org(OrgName),
}

impl MarkTarget {
    /// The `policy_marks.target` key: `owner/name` or `owner`.
    pub fn key(&self) -> String {
        match self {
            Self::Repo(repo) => repo.to_string(),
            Self::Org(org) => org.to_string(),
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::Repo(_) => "repo",
            Self::Org(_) => "org",
        }
    }

    pub fn owner(&self) -> &str {
        match self {
            Self::Repo(repo) => repo.owner(),
            Self::Org(org) => org.as_str(),
        }
    }
}

impl FromStr for MarkTarget {
    type Err = ParseError;

    /// `owner/name` is a repository; a bare `owner` is an org.
    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let rest = strip_github_prefix(input.trim()).trim_end_matches('/');
        if rest.contains('/') {
            Ok(Self::Repo(input.parse()?))
        } else {
            Ok(Self::Org(input.parse()?))
        }
    }
}

impl fmt::Display for MarkTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.key())
    }
}

/// An agent-written AI policy mark (§5). The CLI only validates format.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyMark {
    pub target: MarkTarget,
    pub stance: Stance,
    #[serde(default)]
    pub scope: PolicyScope,
    #[serde(default)]
    pub requirements: Vec<Requirement>,
    /// At least one entry.
    pub evidence: Vec<Evidence>,
    /// The agent's one-line summary, shown verbatim in reminders.
    pub note: String,
    pub marked_by: AgentId,
    pub marked_at: DateTime<Utc>,
    /// After this the mark is `review_due`; it is never deleted or rewritten (§5).
    pub expires_at: DateTime<Utc>,
}

impl PolicyMark {
    /// Why `gate pr` must block with `human_required` (§8): the scope's
    /// entries (see [`PolicyScope::human_required_reasons`]) plus the
    /// `human_written_description` / `human_written_replies` requirements.
    /// Empty when an agent may open the PR.
    pub fn human_required_reasons(&self) -> Vec<&'static str> {
        let mut why = self.scope.human_required_reasons();
        for requirement in &self.requirements {
            match requirement {
                Requirement::HumanWrittenDescription => {
                    why.push("requirement: human_written_description")
                }
                Requirement::HumanWrittenReplies => why.push("requirement: human_written_replies"),
                _ => {}
            }
        }
        why
    }

    /// Format checks only (§5 step 5): evidence present, note present, short excerpts.
    pub fn validate(&self) -> Result<(), ParseError> {
        let target = self.target.key();
        if self.evidence.is_empty() {
            return Err(ParseError::new(
                "policy mark",
                &target,
                "at least one --evidence is required",
            ));
        }
        if self.note.trim().is_empty() {
            return Err(ParseError::new(
                "policy mark",
                &target,
                "--note is required",
            ));
        }
        if let Some(evidence) = self
            .evidence
            .iter()
            .find(|e| e.excerpt.lines().count() > Evidence::MAX_EXCERPT_LINES)
        {
            return Err(ParseError::new(
                "policy mark",
                &target,
                format!(
                    "the excerpt for {} exceeds {} lines",
                    evidence.path,
                    Evidence::MAX_EXCERPT_LINES
                ),
            ));
        }
        if self.expires_at < self.marked_at {
            return Err(ParseError::new(
                "policy mark",
                &target,
                "expires_at is before marked_at",
            ));
        }
        Ok(())
    }

    pub fn review_due(&self, now: DateTime<Utc>) -> bool {
        now >= self.expires_at
    }
}

open_enum! {
    /// Kind of `policy_mark_history` entry.
    MarkAction, "mark action" {
        Set => "set",
        Unset => "unset",
    }
}

/// One audit row: who changed which judgment, when, based on what (§6).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyMarkEvent {
    pub id: i64,
    pub target: MarkTarget,
    pub action: MarkAction,
    /// The mark written (set) or the mark removed (unset).
    pub mark: Option<PolicyMark>,
    pub agent: AgentId,
    pub at: DateTime<Utc>,
}

/// Raw text of one candidate policy file. No interpretation (§5 step 4).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyFile {
    /// Repository the file came from: the target repo or `{owner}/.github`.
    pub repo: RepoName,
    pub path: String,
    /// Permanent (commit-pinned) link when known, else the blob URL.
    pub url: String,
    /// `None` when only the list was requested (`gate start`, §8).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default)]
    pub truncated: bool,
    /// Size of the full file in bytes.
    #[serde(default)]
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha: Option<String>,
    pub fetched_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Repositories and issues (§6)
// ---------------------------------------------------------------------------

/// Contributability metrics; the most expensive data, computed only for
/// repositories that passed the policy check (§7).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RepoActivity {
    /// Merged / closed PRs from non-members, `0.0..=1.0`.
    pub external_pr_merge_rate: Option<f64>,
    pub median_first_response_secs: Option<u64>,
    pub maintainer_last_active: Option<DateTime<Utc>>,
    pub sample_size: u32,
    pub computed_at: DateTime<Utc>,
}

/// Repository metadata plus optional activity.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RepoProfile {
    pub repo: RepoName,
    #[serde(default)]
    pub description: Option<String>,
    pub archived: bool,
    pub fork: bool,
    #[serde(default)]
    pub license: Option<String>,
    #[serde(default)]
    pub default_branch: Option<String>,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub topics: Vec<String>,
    #[serde(default)]
    pub stars: u64,
    #[serde(default)]
    pub pushed_at: Option<DateTime<Utc>>,
    pub fetched_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<RepoActivity>,
}

open_enum! {
    /// GitHub issue state.
    IssueState, "issue state" {
        Open => "open",
        Closed => "closed",
    }
}

/// Index fields of an issue; bodies are fetched on demand (§6).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueRow {
    pub issue: IssueRef,
    pub title: String,
    pub state: IssueState,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub assignees: Vec<String>,
    #[serde(default)]
    pub comments: u32,
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
    pub fetched_at: DateTime<Utc>,
}

/// A raw issue comment, passed through for the agent to read.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueComment {
    pub author: Option<String>,
    #[serde(default)]
    pub author_association: Option<String>,
    pub body: String,
    pub created_at: DateTime<Utc>,
    pub url: String,
}

/// A PR linked to an issue through timeline `cross-referenced` / `connected` events.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkedPr {
    pub url: String,
    /// Timeline event name, e.g. `cross-referenced`.
    pub event: String,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub head_ref: Option<String>,
}

/// An issue with its body, recent comments, and linked PRs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueDetail {
    pub row: IssueRow,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub comments: Vec<IssueComment>,
    #[serde(default)]
    pub linked_prs: Vec<LinkedPr>,
    /// The number is a pull request (GitHub's issues API answers for PRs too).
    /// `issue show` still shows it; gates refuse it.
    #[serde(default)]
    pub is_pull_request: bool,
}

// ---------------------------------------------------------------------------
// Claims and gates (§8, §9)
// ---------------------------------------------------------------------------

open_enum! {
    /// Lifecycle of a local claim.
    ClaimStatus, "claim status" {
        Active => "active",
        Released => "released",
        Expired => "expired",
        Submitted => "submitted",
    }
}

/// A local, lease-based claim on an issue. Never written to GitHub (§9).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claim {
    pub issue: IssueRef,
    pub agent: AgentId,
    pub claimed_at: DateTime<Utc>,
    pub lease_until: DateTime<Utc>,
    pub status: ClaimStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

/// Result of trying to claim or renew.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ClaimOutcome {
    Acquired {
        claim: Claim,
    },
    Renewed {
        claim: Claim,
    },
    /// Another agent holds an unexpired lease.
    HeldByOther {
        claim: Claim,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateKind {
    Start,
    Pr,
}

impl GateKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Pr => "pr",
        }
    }
}

/// Gate result (§8). `Error` is fail-closed: network or quota trouble.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateVerdict {
    Pass,
    Blocked,
    Error,
}

impl GateVerdict {
    /// §10: `0` pass, `20` blocked, `1` error.
    pub fn exit_code(self) -> u8 {
        match self {
            Self::Pass => 0,
            Self::Blocked => 20,
            Self::Error => 1,
        }
    }
}

open_enum! {
    /// Why a gate blocked (§8).
    BlockReason, "block reason" {
        PolicyUnmarked => "policy_unmarked",
        PolicyBans => "policy_bans",
        HumanRequired => "human_required",
        ClaimedByOther => "claimed_by_other",
        /// `gate pr` without a held claim.
        ClaimNotHeld => "claim_not_held",
        PrExists => "pr_exists",
        IssueClosed => "issue_closed",
        AssignedToOther => "assigned_to_other",
        RepoArchived => "repo_archived",
        ForbiddenLabel => "forbidden_label",
        /// Prior maintainer approval is required and none was found (`gate pr` only).
        ApprovalMissing => "approval_missing",
        /// The issue carries an active agent-written `skip` mark (§5b).
        IssueSkipped => "issue_skipped",
    }
}

/// One blocking finding with a human-readable detail.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateFinding {
    pub reason: BlockReason,
    pub detail: String,
}

open_enum! {
    /// How much attention a report-only gate observation deserves. Never blocks.
    ObservationSeverity, "observation severity" {
        Info => "info",
        Warning => "warning",
    }
}

open_enum! {
    /// What a report-only gate observation is about (§8: heuristics are reported, not judged).
    ObservationKind, "observation kind" {
        /// A recent comment reads like someone claiming the work (heuristic).
        CommentClaim => "comment_claim",
        /// The issue is assigned to the authenticated GitHub user.
        AssignedToYou => "assigned_to_you",
        /// The mark says maintainer approval is needed before work or a PR.
        ApprovalRequired => "approval_required",
        /// A maintainer comment the agent should read for approval.
        ApprovalEvidence => "approval_evidence",
        /// The approval URL the agent supplied with `--approval-url`.
        ApprovalUrl => "approval_url",
        /// A linked PR that is the agent's own `--branch`, excluded from `pr_exists`.
        OwnPrExcluded => "own_pr_excluded",
        /// A linked PR that was closed without merging; not blocking.
        ClosedPr => "closed_pr",
        /// The policy mark is past its review date.
        MarkReviewDue => "mark_review_due",
        /// An issue mark that does not block (shortlist, or an expired skip).
        IssueMark => "issue_mark",
        /// A policy file whose text was truncated.
        PolicyFileTruncated => "policy_file_truncated",
    }
}

/// A report-only gate observation (`severity` is `info` or `warning`; it never blocks).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateObservation {
    pub kind: ObservationKind,
    pub severity: ObservationSeverity,
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// Raw, live GitHub data handed to the agent; the agent decides from this,
/// not from the CLI's summary (§8).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct GateRaw {
    /// Body, the last `gate_comments` comments verbatim, and timeline-linked PRs.
    pub issue: Option<IssueDetail>,
    pub repo: Option<RepoProfile>,
    /// The effective policy mark: note, scope, requirements, evidence excerpts and pinned URLs.
    pub policy_mark: Option<PolicyMark>,
    /// The agent-written issue mark, if any (§5b).
    #[serde(default)]
    pub issue_mark: Option<crate::issue_marks::IssueMark>,
    /// `gate start`: list only (`text` is `None`); `gate pr`: full text.
    #[serde(default)]
    pub policy_files: Vec<PolicyFile>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GateReport {
    pub kind: GateKind,
    pub issue: IssueRef,
    pub agent: AgentId,
    pub verdict: GateVerdict,
    #[serde(default)]
    pub blocked_by: Vec<GateFinding>,
    /// Report-only heuristics: comment-based claims, approval evidence, own PRs (§8, §11.4).
    #[serde(default)]
    pub observations: Vec<GateObservation>,
    /// PR requirements from the mark: a preview on `start`, the full list on `pr`.
    #[serde(default)]
    pub requirements: Vec<Requirement>,
    /// `gate pr` only: every requirement, restricted scope, approval, and policy
    /// file to read, as one checklist line each.
    #[serde(default)]
    pub checklist: Vec<String>,
    #[serde(default)]
    pub claim: Option<Claim>,
    /// The authenticated GitHub login (one live `GET /user`, never cached).
    #[serde(default)]
    pub viewer: Option<String>,
    /// `gate pr --approval-url`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_url: Option<String>,
    /// Set when `verdict` is `error` (only in `gate_log`; the CLI exits 1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub raw: GateRaw,
    pub as_of: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Reminders (§5)
// ---------------------------------------------------------------------------

open_enum! {
    ReminderKind, "reminder kind" {
        /// A mark applies; `review_due` may be set.
        Marked => "marked",
        /// No mark; `hint` says how to create one.
        Unmarked => "unmarked",
        /// Non-authoritative hint, e.g. from an imported external list (§11.2).
        Hint => "hint",
        /// Listing entry for `policy due`.
        ReviewDue => "review_due",
        /// An agent-written issue mark applies (§5b); details in `Reminder::issue_mark`.
        IssueMarked => "issue_marked",
    }
}

/// Attached to `meta.reminders` by every command that touches a repository.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reminder {
    pub kind: ReminderKind,
    pub repo: RepoName,
    /// The mark that applied: the repo's own, or its org's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<MarkTarget>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stance: Option<Stance>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marked_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marked_by: Option<AgentId>,
    #[serde(default)]
    pub evidence_urls: Vec<String>,
    #[serde(default)]
    pub review_due: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// Set for `issue_marked` reminders (§5b).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue_mark: Option<crate::issue_marks::IssueMarkReminder>,
}
