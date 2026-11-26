//! Reminder construction (§5 steps 2-3). Every command that touches a
//! repository calls [`reminder_for`] and adds the result to `meta.reminders`.

use crate::models::{PolicyMark, Reminder, ReminderKind, RepoName};
use chrono::{DateTime, Utc};

/// `marked` (with `review_due` once expired) when a mark applies, else `unmarked` with a hint.
pub fn reminder_for(repo: &RepoName, mark: Option<&PolicyMark>, now: DateTime<Utc>) -> Reminder {
    match mark {
        Some(mark) => Reminder {
            kind: ReminderKind::Marked,
            repo: repo.clone(),
            target: Some(mark.target.clone()),
            stance: Some(mark.stance.clone()),
            note: Some(mark.note.clone()),
            marked_at: Some(mark.marked_at),
            marked_by: Some(mark.marked_by.clone()),
            evidence_urls: mark.evidence.iter().map(|e| e.url.clone()).collect(),
            review_due: mark.review_due(now),
            hint: None,
        },
        None => Reminder {
            kind: ReminderKind::Unmarked,
            repo: repo.clone(),
            target: None,
            stance: None,
            note: None,
            marked_at: None,
            marked_by: None,
            evidence_urls: Vec::new(),
            review_due: false,
            hint: Some(format!(
                "run `contrib-scout policy files {repo}`, read the text, then record it with `contrib-scout policy set`"
            )),
        },
    }
}

/// The prominent human-readable line, e.g.
/// `⚠ owner/repo: Bans AI — "<note>" (marked 2026-11-20)`.
pub fn human_line(reminder: &Reminder) -> String {
    match (&reminder.kind, &reminder.stance) {
        (ReminderKind::Marked | ReminderKind::ReviewDue, Some(stance)) => {
            let mut line = format!(
                "⚠ {}: {} — \"{}\"",
                reminder.repo,
                stance.label(),
                reminder.note.as_deref().unwrap_or_default()
            );
            if let Some(at) = reminder.marked_at {
                line.push_str(&format!(" (marked {})", at.format("%Y-%m-%d")));
            }
            if let Some(target) = reminder
                .target
                .as_ref()
                .filter(|t| t.key() != reminder.repo.to_string())
            {
                line.push_str(&format!(" [via {} {}]", target.kind(), target));
            }
            if reminder.review_due {
                line.push_str(" [review due]");
            }
            line
        }
        _ => format!(
            "⚠ {}: AI policy not marked — {}",
            reminder.repo,
            reminder.hint.as_deref().unwrap_or("no hint")
        ),
    }
}
