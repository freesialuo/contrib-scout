//! Issue marks: storage, history, index snapshot, changed_since_mark, expiry (§5b).

use chrono::{DateTime, TimeDelta, TimeZone, Utc};
use contrib_scout_core::index::parse_issue_row;
use contrib_scout_core::issue_marks::{
    changed_since_mark, issue_reminders, mark_issue, IssueMarkAction, IssueMarkFilter,
    IssueVerdict, NewIssueMark, SkipReason,
};
use contrib_scout_core::models::ReminderKind;
use contrib_scout_core::policy::human_line;
use contrib_scout_core::store::Store;
use contrib_scout_core::{AgentId, IssueRef, ScoutError};
use serde_json::json;

fn at(day: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, day, 12, 0, 0).unwrap()
}

fn agent() -> AgentId {
    "agent-a".parse().unwrap()
}

fn issue(text: &str) -> IssueRef {
    text.parse().unwrap()
}

fn index(store: &mut Store, reference: &IssueRef, updated: DateTime<Utc>, comments: u32) {
    let raw = json!({
        "number": reference.number,
        "title": "t",
        "state": "open",
        "labels": [],
        "assignees": [],
        "comments": comments,
        "updated_at": updated.to_rfc3339(),
    });
    let row = parse_issue_row(&reference.repo, &raw, updated).unwrap();
    store.save_issues(&[(row, raw)]).unwrap();
}

fn skip(reference: &IssueRef) -> NewIssueMark {
    NewIssueMark {
        issue: reference.clone(),
        verdict: IssueVerdict::Skip,
        reason: Some(SkipReason::TooLarge),
        note: "Needs a redesign of the parser".into(),
        expires_at: None,
    }
}

#[test]
fn put_get_delete_with_history() {
    let mut store = Store::memory().unwrap();
    let reference = issue("o/r#7");
    assert_eq!(store.issue_mark(&reference).unwrap(), None);

    let mark = mark_issue(&mut store, skip(&reference), &agent(), at(1)).unwrap();
    assert_eq!(store.issue_mark(&reference).unwrap(), Some(mark.clone()));
    // Case-insensitive lookup, like GitHub names.
    assert!(store.issue_mark(&issue("O/R#7")).unwrap().is_some());

    let shortlist = NewIssueMark {
        verdict: IssueVerdict::Shortlist,
        reason: None,
        note: "Actually small".into(),
        ..skip(&reference)
    };
    let overwritten = mark_issue(&mut store, shortlist, &agent(), at(2)).unwrap();
    assert_eq!(overwritten.verdict, IssueVerdict::Shortlist);

    let removed = store
        .delete_issue_mark(&reference, &agent(), at(3))
        .unwrap();
    assert_eq!(removed, Some(overwritten.clone()));
    assert_eq!(store.issue_mark(&reference).unwrap(), None);
    assert_eq!(
        store
            .delete_issue_mark(&reference, &agent(), at(4))
            .unwrap(),
        None
    );

    let history = store.issue_mark_history(&reference).unwrap();
    let actions: Vec<_> = history.iter().map(|e| e.action).collect();
    assert_eq!(
        actions,
        [
            IssueMarkAction::Mark,
            IssueMarkAction::Mark,
            IssueMarkAction::Unmark
        ]
    );
    assert_eq!(history[0].mark.as_ref(), Some(&mark));
    assert_eq!(history[2].mark.as_ref(), Some(&overwritten));
    assert_eq!(history[2].at, at(3));
}

#[test]
fn validation_requires_reason_for_skip_only_and_a_note() {
    let mut store = Store::memory().unwrap();
    let reference = issue("o/r#1");
    let cases = [
        NewIssueMark {
            reason: None,
            ..skip(&reference)
        },
        NewIssueMark {
            verdict: IssueVerdict::Shortlist,
            ..skip(&reference)
        },
        NewIssueMark {
            note: "  ".into(),
            ..skip(&reference)
        },
        NewIssueMark {
            expires_at: Some(at(1)),
            ..skip(&reference)
        },
    ];
    for case in cases {
        let error = mark_issue(&mut store, case, &agent(), at(2)).unwrap_err();
        assert!(
            matches!(ScoutError::find(&error), Some(ScoutError::Usage(_))),
            "{error:#}"
        );
    }
    assert!(store.issue_mark_history(&reference).unwrap().is_empty());
    assert_eq!(
        "needs-special-env".parse::<SkipReason>().unwrap(),
        SkipReason::NeedsSpecialEnv
    );
    assert!("lazy".parse::<SkipReason>().is_err());
    assert!("maybe".parse::<IssueVerdict>().is_err());
}

#[test]
fn snapshot_comes_from_the_index_and_detects_changes() {
    let mut store = Store::memory().unwrap();
    let reference = issue("o/r#7");
    index(&mut store, &reference, at(1), 2);

    let mark = mark_issue(&mut store, skip(&reference), &agent(), at(2)).unwrap();
    assert_eq!(mark.seen_updated_at, Some(at(1)));
    assert_eq!(mark.seen_comments, Some(2));
    let status = store.issue_mark_status(&reference, at(2)).unwrap().unwrap();
    assert_eq!(status.changed_since_mark, Some(false));

    // A sync brings a new comment.
    index(&mut store, &reference, at(3), 3);
    let status = store.issue_mark_status(&reference, at(3)).unwrap().unwrap();
    assert_eq!(status.changed_since_mark, Some(true));
    assert_eq!(status.current_comments, Some(3));

    // The reminder carries it and the human line shows it.
    let reminders = issue_reminders(&store, &reference, at(3)).unwrap();
    assert_eq!(reminders.len(), 2);
    assert_eq!(reminders[0].kind, ReminderKind::Unmarked);
    let issue_reminder = &reminders[1];
    assert_eq!(issue_reminder.kind, ReminderKind::IssueMarked);
    let details = issue_reminder.issue_mark.as_ref().unwrap();
    assert_eq!(details.changed_since_mark, Some(true));
    assert_eq!(details.verdict, IssueVerdict::Skip);
    let line = human_line(issue_reminder);
    assert!(line.contains("o/r#7: marked skip (too_large)"), "{line}");
    assert!(line.contains("[changed since mark]"), "{line}");
    let json = serde_json::to_value(issue_reminder).unwrap();
    assert_eq!(json["kind"], "issue_marked");
    assert_eq!(json["issue_mark"]["changed_since_mark"], true);
}

#[test]
fn unindexed_issue_has_no_snapshot() {
    let mut store = Store::memory().unwrap();
    let reference = issue("o/r#8");
    let mark = mark_issue(&mut store, skip(&reference), &agent(), at(2)).unwrap();
    assert_eq!((mark.seen_updated_at, mark.seen_comments), (None, None));
    assert_eq!(changed_since_mark(&mark, None), None);
    // Indexed later: changed only if updated after the mark.
    index(&mut store, &reference, at(1), 0);
    let status = store.issue_mark_status(&reference, at(3)).unwrap().unwrap();
    assert_eq!(status.changed_since_mark, Some(false));
    index(&mut store, &reference, at(4), 0);
    let status = store.issue_mark_status(&reference, at(5)).unwrap().unwrap();
    assert_eq!(status.changed_since_mark, Some(true));
}

#[test]
fn expiry_and_list_filters() {
    let mut store = Store::memory().unwrap();
    let expiring = NewIssueMark {
        expires_at: Some(at(10)),
        ..skip(&issue("o/r#1"))
    };
    mark_issue(&mut store, expiring, &agent(), at(1)).unwrap();
    mark_issue(&mut store, skip(&issue("o/other#2")), &agent(), at(2)).unwrap();
    let shortlist = NewIssueMark {
        verdict: IssueVerdict::Shortlist,
        reason: None,
        ..skip(&issue("o/r#3"))
    };
    mark_issue(&mut store, shortlist, &agent(), at(3)).unwrap();

    let all = store.issue_marks(&IssueMarkFilter::default()).unwrap();
    let refs: Vec<String> = all.iter().map(|m| m.issue.to_string()).collect();
    assert_eq!(refs, ["o/r#3", "o/other#2", "o/r#1"], "newest first");

    let skips = store
        .issue_marks(&IssueMarkFilter {
            verdict: Some(IssueVerdict::Skip),
            ..IssueMarkFilter::default()
        })
        .unwrap();
    assert_eq!(skips.len(), 2);
    let in_repo = store
        .issue_marks(&IssueMarkFilter {
            repo: Some("O/R".parse().unwrap()),
            ..IssueMarkFilter::default()
        })
        .unwrap();
    assert_eq!(in_repo.len(), 2);

    // Expired marks are kept, but are not active.
    let later = at(10) + TimeDelta::seconds(1);
    let active = store
        .issue_marks(&IssueMarkFilter {
            active_at: Some(later),
            ..IssueMarkFilter::default()
        })
        .unwrap();
    assert_eq!(active.len(), 2);
    let expired = store
        .issue_mark_status(&issue("o/r#1"), later)
        .unwrap()
        .unwrap();
    assert!(!expired.active);
    assert!(
        store
            .issue_mark_status(&issue("o/r#1"), at(9))
            .unwrap()
            .unwrap()
            .active
    );
    let reminder = &issue_reminders(&store, &issue("o/r#1"), later).unwrap()[1];
    assert!(human_line(reminder).contains("[expired]"));
}

#[test]
fn tracked_repositories() {
    let mut store = Store::memory().unwrap();
    let repo = "o/r".parse().unwrap();
    assert!(store.add_tracked(&repo, Some(&agent()), at(1)).unwrap());
    assert!(!store
        .add_tracked(&"O/R".parse().unwrap(), None, at(2))
        .unwrap());
    let tracked = store.tracked_repos().unwrap();
    assert_eq!(tracked.len(), 1);
    assert_eq!(tracked[0].added_by, Some(agent()));
    assert!(store.remove_tracked(&repo).unwrap());
    assert!(!store.remove_tracked(&repo).unwrap());
    assert!(store.tracked_repos().unwrap().is_empty());
}
