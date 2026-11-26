//! Agent-written issue marks (design §5b). Step 5 owns this file.
//!
//! Same idea as policy marks, one issue at a time: the CLI never judges an
//! issue, it stores the agent's verdict and reminds later readers. A mark
//! records a snapshot of the issue as the local index saw it at mark time
//! (`seen_updated_at`, `seen_comments`); [`changed_since_mark`] compares that
//! snapshot with the current index row without any extra request.
//!
//! Storage lives in `store/issue_marks.rs`. Column semantics (read directly by
//! `candidates`): `issue_ref` is `owner/repo#N`, `verdict` is `skip` or
//! `shortlist`, `expires_at` NULL means "never expires", and
//! `seen_updated_at` / `seen_comments` are the snapshot (NULL when the issue
//! was not indexed at mark time).

use crate::error::{Result, ScoutError};
use crate::models::{AgentId, IssueRef, IssueRow, ParseError, Reminder, ReminderKind};
use crate::policy::reminders::reminder_for;
use crate::store::Store;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::str::FromStr;

/// The agent's verdict on an issue.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueVerdict {
    /// Not worth doing; excluded from `candidates` / `discover` by default.
    Skip,
    /// Worth doing; kept for a later agent and ranked first.
    Shortlist,
}

impl IssueVerdict {
    pub const KNOWN: &'static [&'static str] = &["skip", "shortlist"];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Skip => "skip",
            Self::Shortlist => "shortlist",
        }
    }
}

impl FromStr for IssueVerdict {
    type Err = ParseError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        match input.trim().to_ascii_lowercase().as_str() {
            "skip" => Ok(Self::Skip),
            "shortlist" => Ok(Self::Shortlist),
            _ => Err(ParseError {
                what: "issue verdict",
                input: input.to_owned(),
                reason: format!("expected one of {}", Self::KNOWN.join(", ")),
            }),
        }
    }
}

impl fmt::Display for IssueVerdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why an issue was skipped. Open enum: values written by a newer version
/// are kept verbatim as `Unknown`; CLI input is strict.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum SkipReason {
    TooLarge,
    Unclear,
    NeedsSpecialEnv,
    OutOfScope,
    Duplicate,
    MaintainerUnresponsive,
    AlreadyAddressed,
    Other,
    Unknown(String),
}

impl SkipReason {
    pub const KNOWN: &'static [&'static str] = &[
        "too_large",
        "unclear",
        "needs_special_env",
        "out_of_scope",
        "duplicate",
        "maintainer_unresponsive",
        "already_addressed",
        "other",
    ];

    pub fn as_str(&self) -> &str {
        match self {
            Self::TooLarge => "too_large",
            Self::Unclear => "unclear",
            Self::NeedsSpecialEnv => "needs_special_env",
            Self::OutOfScope => "out_of_scope",
            Self::Duplicate => "duplicate",
            Self::MaintainerUnresponsive => "maintainer_unresponsive",
            Self::AlreadyAddressed => "already_addressed",
            Self::Other => "other",
            Self::Unknown(other) => other,
        }
    }

    /// Lenient conversion for stored data.
    pub fn from_stored(value: &str) -> Self {
        match value {
            "too_large" => Self::TooLarge,
            "unclear" => Self::Unclear,
            "needs_special_env" => Self::NeedsSpecialEnv,
            "out_of_scope" => Self::OutOfScope,
            "duplicate" => Self::Duplicate,
            "maintainer_unresponsive" => Self::MaintainerUnresponsive,
            "already_addressed" => Self::AlreadyAddressed,
            "other" => Self::Other,
            other => Self::Unknown(other.to_owned()),
        }
    }
}

impl FromStr for SkipReason {
    type Err = ParseError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let normalized = input.trim().to_ascii_lowercase().replace('-', "_");
        match Self::from_stored(&normalized) {
            Self::Unknown(_) => Err(ParseError {
                what: "skip reason",
                input: input.to_owned(),
                reason: format!("expected one of {}", Self::KNOWN.join(", ")),
            }),
            known => Ok(known),
        }
    }
}

impl fmt::Display for SkipReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for SkipReason {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for SkipReason {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self::from_stored(&String::deserialize(deserializer)?))
    }
}

/// One agent-written mark on an issue (§5b).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueMark {
    pub issue: IssueRef,
    pub verdict: IssueVerdict,
    /// Required for `skip`, absent for `shortlist`.
    #[serde(default)]
    pub reason: Option<SkipReason>,
    /// One sentence, shown verbatim in reminders.
    pub note: String,
    /// Index snapshot at mark time; `None` when the issue was not indexed.
    #[serde(default)]
    pub seen_updated_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub seen_comments: Option<u32>,
    pub marked_by: AgentId,
    pub marked_at: DateTime<Utc>,
    /// `None`: never expires (the default).
    #[serde(default)]
    pub expires_at: Option<DateTime<Utc>>,
}

impl IssueMark {
    /// Format checks only; the CLI never judges the verdict itself.
    pub fn validate(&self) -> Result<(), ScoutError> {
        if self.note.trim().is_empty() {
            return Err(ScoutError::Usage("an issue mark needs a --note".into()));
        }
        match (self.verdict, &self.reason) {
            (IssueVerdict::Skip, None) => Err(ScoutError::Usage(format!(
                "--verdict skip needs --reason (one of {})",
                SkipReason::KNOWN.join(", ")
            ))),
            (IssueVerdict::Shortlist, Some(_)) => Err(ScoutError::Usage(
                "--reason applies to --verdict skip only".into(),
            )),
            _ => Ok(()),
        }?;
        if let Some(expires_at) = self.expires_at {
            if expires_at <= self.marked_at {
                return Err(ScoutError::Usage("--until must be in the future".into()));
            }
        }
        Ok(())
    }

    /// Active while `expires_at` is unset or still in the future.
    pub fn is_active(&self, now: DateTime<Utc>) -> bool {
        self.expires_at.is_none_or(|expires_at| expires_at > now)
    }
}

/// What an agent supplies to `issue mark`; the snapshot and authorship are
/// filled in by [`mark_issue`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewIssueMark {
    pub issue: IssueRef,
    pub verdict: IssueVerdict,
    pub reason: Option<SkipReason>,
    pub note: String,
    pub expires_at: Option<DateTime<Utc>>,
}

/// `mark` (insert or overwrite) or `unmark`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueMarkAction {
    Mark,
    Unmark,
}

impl IssueMarkAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mark => "mark",
            Self::Unmark => "unmark",
        }
    }
}

/// One `issue_mark_history` row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueMarkEvent {
    pub id: i64,
    pub issue: IssueRef,
    pub action: IssueMarkAction,
    /// The written mark (`mark`) or the removed one (`unmark`).
    pub mark: Option<IssueMark>,
    pub agent: AgentId,
    pub at: DateTime<Utc>,
}

/// Filter for [`Store::issue_marks`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IssueMarkFilter {
    pub verdict: Option<IssueVerdict>,
    pub repo: Option<crate::models::RepoName>,
    /// `Some(now)`: only marks active at `now`.
    pub active_at: Option<DateTime<Utc>>,
}

/// A mark compared with the current index row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueMarkStatus {
    pub mark: IssueMark,
    pub active: bool,
    /// `None` when the issue is not in the local index.
    pub changed_since_mark: Option<bool>,
    #[serde(default)]
    pub current_updated_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub current_comments: Option<u32>,
}

impl IssueMarkStatus {
    pub fn new(mark: IssueMark, row: Option<&IssueRow>, now: DateTime<Utc>) -> Self {
        Self {
            active: mark.is_active(now),
            changed_since_mark: changed_since_mark(&mark, row),
            current_updated_at: row.map(|row| row.updated_at),
            current_comments: row.map(|row| row.comments),
            mark,
        }
    }
}

/// Whether the indexed issue changed after it was marked (§5b), with no request:
/// - snapshot present: `updated_at` or the comment count differs from it;
/// - no snapshot (not indexed at mark time): the issue was updated after `marked_at`;
/// - not indexed now: `None` (unknown).
pub fn changed_since_mark(mark: &IssueMark, row: Option<&IssueRow>) -> Option<bool> {
    let row = row?;
    Some(match (mark.seen_updated_at, mark.seen_comments) {
        (Some(updated_at), comments) => {
            row.updated_at != updated_at || comments.is_some_and(|c| c != row.comments)
        }
        (None, _) => row.updated_at > mark.marked_at,
    })
}

/// Validates, snapshots the issue from the local index, and stores the mark
/// (upsert + history). Never touches the network.
pub fn mark_issue(
    store: &mut Store,
    new: NewIssueMark,
    agent: &AgentId,
    now: DateTime<Utc>,
) -> Result<IssueMark> {
    let row = store.issue(&new.issue)?.map(|cached| cached.data);
    let mark = IssueMark {
        issue: new.issue,
        verdict: new.verdict,
        reason: new.reason,
        note: new.note.trim().to_owned(),
        seen_updated_at: row.as_ref().map(|row| row.updated_at),
        seen_comments: row.as_ref().map(|row| row.comments),
        marked_by: agent.clone(),
        marked_at: now,
        expires_at: new.expires_at,
    };
    mark.validate()?;
    store.put_issue_mark(&mark)?;
    Ok(mark)
}

/// Issue-level details carried by an `issue_marked` reminder.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueMarkReminder {
    pub issue: IssueRef,
    pub verdict: IssueVerdict,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<SkipReason>,
    pub active: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
    /// `None` when the issue is not indexed locally.
    #[serde(default)]
    pub changed_since_mark: Option<bool>,
}

/// The `issue_marked` reminder for a mark status.
pub fn reminder(status: &IssueMarkStatus) -> Reminder {
    let mark = &status.mark;
    Reminder {
        kind: ReminderKind::IssueMarked,
        repo: mark.issue.repo.clone(),
        target: None,
        stance: None,
        note: Some(mark.note.clone()),
        marked_at: Some(mark.marked_at),
        marked_by: Some(mark.marked_by.clone()),
        evidence_urls: Vec::new(),
        review_due: !status.active,
        hint: None,
        issue_mark: Some(IssueMarkReminder {
            issue: mark.issue.clone(),
            verdict: mark.verdict,
            reason: mark.reason.clone(),
            active: status.active,
            expires_at: mark.expires_at,
            changed_since_mark: status.changed_since_mark,
        }),
    }
}

/// Every reminder for one issue: the repository's policy reminder (marked or
/// unmarked), then the issue mark's reminder if one exists. Zero requests.
pub fn issue_reminders(
    store: &Store,
    issue: &IssueRef,
    now: DateTime<Utc>,
) -> Result<Vec<Reminder>> {
    let policy = store.effective_mark(&issue.repo)?;
    let mut reminders = vec![reminder_for(&issue.repo, policy.as_ref(), now)];
    if let Some(status) = store.issue_mark_status(issue, now)? {
        reminders.push(reminder(&status));
    }
    Ok(reminders)
}

/// Human line for an `issue_marked` reminder, e.g.
/// `⚠ o/r#7: marked skip (too_large) — "<note>" (marked 2026-11-20 by agent-a) [changed since mark]`.
pub fn human_line(reminder: &Reminder, issue: &IssueMarkReminder) -> String {
    let symbol = match issue.verdict {
        IssueVerdict::Skip => "⚠",
        IssueVerdict::Shortlist => "★",
    };
    let mut line = format!("{symbol} {}: marked {}", issue.issue, issue.verdict);
    if let Some(reason) = &issue.reason {
        line.push_str(&format!(" ({reason})"));
    }
    line.push_str(&format!(
        " — \"{}\"",
        reminder.note.as_deref().unwrap_or_default()
    ));
    if let Some(at) = reminder.marked_at {
        line.push_str(&format!(" (marked {}", at.format("%Y-%m-%d")));
        if let Some(by) = &reminder.marked_by {
            line.push_str(&format!(" by {by}"));
        }
        line.push(')');
    }
    if issue.changed_since_mark == Some(true) {
        line.push_str(" [changed since mark]");
    }
    if !issue.active {
        line.push_str(" [expired]");
    }
    line
}
