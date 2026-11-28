//! `StoreMarks` and store-backed reminders (§5, §13).

use chrono::{DateTime, TimeDelta, TimeZone, Utc};
use contrib_scout_core::config::Config;
use contrib_scout_core::models::{
    Evidence, MarkAction, MarkTarget, PolicyMark, PolicyScope, ReminderKind, Stance,
};
use contrib_scout_core::policy::{
    expires_at, human_line, repo_reminder, repo_reminders, PolicyMarks, StoreMarks,
};
use contrib_scout_core::store::{SharedStore, Store};
use contrib_scout_core::{AgentId, RepoName, ScoutError};

fn at(day: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, day, 12, 0, 0).unwrap()
}

fn agent() -> AgentId {
    "agent-a".parse().unwrap()
}

fn mark(target: &str, stance: Stance, marked_at: DateTime<Utc>) -> PolicyMark {
    let config = Config::default();
    PolicyMark {
        target: target.parse().unwrap(),
        expires_at: expires_at(&config, &stance, marked_at),
        stance,
        scope: PolicyScope::default(),
        requirements: Vec::new(),
        evidence: vec![Evidence {
            path: "CONTRIBUTING.md".into(),
            lines: None,
            url: format!("https://github.com/{target}/blob/abc/CONTRIBUTING.md"),
            excerpt: "No AI-generated pull requests.".into(),
        }],
        note: format!("{target} note"),
        marked_by: agent(),
        marked_at,
    }
}

fn service(store: &SharedStore, now: DateTime<Utc>) -> StoreMarks {
    StoreMarks::new(store.clone(), Config::default(), now)
}

#[test]
fn ttl_follows_stance() {
    let config = Config::default();
    assert_eq!(
        expires_at(&config, &Stance::Bans, at(1)) - at(1),
        TimeDelta::days(90)
    );
    assert_eq!(
        expires_at(&config, &Stance::Disclose, at(1)) - at(1),
        TimeDelta::days(30)
    );
    assert_eq!(
        expires_at(&config, &Stance::NoneFound, at(1)) - at(1),
        TimeDelta::days(14)
    );
}

#[test]
fn set_validates_before_writing() {
    let store = Store::memory().unwrap().shared();
    let marks = service(&store, at(1));
    let mut bad = mark("acme/widget", Stance::Bans, at(1));
    bad.evidence.clear();
    let error = marks.set(&bad).unwrap_err();
    assert!(matches!(
        ScoutError::find(&error),
        Some(ScoutError::Usage(_))
    ));

    let mut long = mark("acme/widget", Stance::Bans, at(1));
    long.evidence[0].excerpt = "a\nb\nc\nd".into();
    assert!(marks.set(&long).is_err());

    let mut blank = mark("acme/widget", Stance::Bans, at(1));
    blank.note = "  ".into();
    assert!(marks.set(&blank).is_err());

    let target: MarkTarget = "acme/widget".parse().unwrap();
    assert_eq!(marks.get_target(&target).unwrap(), None);
    assert!(marks.history(&target).unwrap().is_empty());
}

#[test]
fn set_get_unset_history_due() {
    let store = Store::memory().unwrap().shared();
    let marks = service(&store, at(20));
    let repo: RepoName = "acme/widget".parse().unwrap();
    let repo_target = MarkTarget::Repo(repo.clone());
    let org_target: MarkTarget = "acme".parse().unwrap();

    let org = mark("acme", Stance::Bans, at(1));
    marks.set(&org).unwrap();
    assert_eq!(
        marks.get(&repo).unwrap(),
        Some(org.clone()),
        "org mark applies"
    );
    assert_eq!(marks.get_target(&repo_target).unwrap(), None);

    let own = mark("acme/widget", Stance::Allows, at(2));
    marks.set(&own).unwrap();
    assert_eq!(
        marks.get(&repo).unwrap(),
        Some(own.clone()),
        "repo overrides org"
    );

    // Allows expires after 14 days (Nov 16), Bans after 90.
    let due = marks.due(at(20)).unwrap();
    assert_eq!(due, vec![own.clone()]);

    let removed = marks.unset(&repo_target, &agent()).unwrap();
    assert_eq!(removed, Some(own.clone()));
    assert_eq!(marks.get(&repo).unwrap(), Some(org.clone()));
    let history = marks.history(&repo_target).unwrap();
    assert_eq!(
        history.iter().map(|e| e.action.clone()).collect::<Vec<_>>(),
        vec![MarkAction::Set, MarkAction::Unset]
    );
    assert_eq!(history[1].at, at(20));
    assert_eq!(history[1].mark, Some(own));
    assert_eq!(marks.history(&org_target).unwrap().len(), 1);
}

#[test]
fn reminders_come_from_the_store() {
    let mut store = Store::memory().unwrap();
    let widget: RepoName = "acme/widget".parse().unwrap();
    let gadget: RepoName = "acme/gadget".parse().unwrap();
    let solo: RepoName = "solo/tool".parse().unwrap();

    // Unmarked: a hint to read and mark.
    let unmarked = repo_reminder(&store, &solo, at(1)).unwrap();
    assert_eq!(unmarked.kind, ReminderKind::Unmarked);
    let hint = unmarked.hint.clone().unwrap();
    assert!(
        hint.contains("contrib-scout policy files solo/tool"),
        "{hint}"
    );
    assert!(hint.contains("policy set"), "{hint}");
    assert!(human_line(&unmarked).contains("not marked"));

    // Org mark covers every repository in the org.
    store.put_mark(&mark("acme", Stance::Bans, at(1))).unwrap();
    let via_org = repo_reminder(&store, &gadget, at(2)).unwrap();
    assert_eq!(via_org.kind, ReminderKind::Marked);
    assert_eq!(via_org.stance, Some(Stance::Bans));
    assert_eq!(via_org.target, Some("acme".parse().unwrap()));
    assert_eq!(via_org.marked_by, Some(agent()));
    assert_eq!(via_org.marked_at, Some(at(1)));
    assert_eq!(
        via_org.evidence_urls,
        vec!["https://github.com/acme/blob/abc/CONTRIBUTING.md".to_owned()]
    );
    assert!(!via_org.review_due);
    let line = human_line(&via_org);
    assert!(line.contains("acme/gadget: Bans AI"), "{line}");
    assert!(line.contains("[via org acme]"), "{line}");

    // Repo mark wins; review_due once expired (Disclose: 30 days).
    store
        .put_mark(&mark("acme/widget", Stance::Disclose, at(1)))
        .unwrap();
    let own = repo_reminder(&store, &widget, at(2)).unwrap();
    assert_eq!(own.stance, Some(Stance::Disclose));
    assert!(!own.review_due);
    let later = at(1) + TimeDelta::days(31);
    let due = repo_reminder(&store, &widget, later).unwrap();
    assert!(due.review_due);
    assert!(human_line(&due).contains("[review due]"));

    // A repository spelled in another case is not "via" its own mark.
    let shouty: RepoName = "Acme/Widget".parse().unwrap();
    let line = human_line(&repo_reminder(&store, &shouty, at(2)).unwrap());
    assert!(line.contains("Requires AI disclosure"), "{line}");
    assert!(!line.contains("[via"), "{line}");

    let all = repo_reminders(&store, &[widget, gadget, solo], at(2)).unwrap();
    assert_eq!(
        all.iter().map(|r| r.kind.clone()).collect::<Vec<_>>(),
        vec![
            ReminderKind::Marked,
            ReminderKind::Marked,
            ReminderKind::Unmarked
        ]
    );
}
