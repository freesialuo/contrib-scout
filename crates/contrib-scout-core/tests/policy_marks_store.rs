//! Policy mark storage: repo-over-org precedence, history, due list (§5, §6).

use chrono::{DateTime, TimeDelta, TimeZone, Utc};
use contrib_scout_core::models::{
    Evidence, MarkAction, MarkTarget, PolicyMark, PolicyScope, Stance,
};
use contrib_scout_core::store::Store;
use contrib_scout_core::{AgentId, RepoName};

fn at(day: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, day, 12, 0, 0).unwrap()
}

fn mark(target: &str, stance: Stance, marked_at: DateTime<Utc>, ttl_days: i64) -> PolicyMark {
    PolicyMark {
        target: target.parse().unwrap(),
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
        marked_by: "agent-a".parse().unwrap(),
        marked_at,
        expires_at: marked_at + TimeDelta::days(ttl_days),
    }
}

#[test]
fn repo_mark_takes_precedence_over_org_mark() {
    let mut store = Store::memory().unwrap();
    let repo: RepoName = "acme/widget".parse().unwrap();
    let other: RepoName = "acme/gadget".parse().unwrap();

    assert_eq!(store.effective_mark(&repo).unwrap(), None);

    store
        .put_mark(&mark("acme", Stance::Bans, at(1), 90))
        .unwrap();
    assert_eq!(
        store.effective_mark(&other).unwrap().unwrap().target,
        "acme".parse::<MarkTarget>().unwrap()
    );

    let own = mark("acme/widget", Stance::Allows, at(2), 14);
    store.put_mark(&own).unwrap();
    assert_eq!(store.effective_mark(&repo).unwrap(), Some(own));
    assert_eq!(
        store.effective_mark(&other).unwrap().unwrap().stance,
        Stance::Bans
    );

    let many = store
        .effective_marks(&[repo.clone(), other.clone(), "solo/x".parse().unwrap()])
        .unwrap();
    assert_eq!(many.len(), 2);
}

#[test]
fn set_overwrites_and_unset_keeps_history() {
    let mut store = Store::memory().unwrap();
    let target: MarkTarget = "acme/widget".parse().unwrap();
    let agent: AgentId = "agent-b".parse().unwrap();

    store
        .put_mark(&mark("acme/widget", Stance::NoneFound, at(1), 14))
        .unwrap();
    store
        .put_mark(&mark("acme/widget", Stance::Restricts, at(3), 30))
        .unwrap();
    assert_eq!(
        store.mark(&target).unwrap().unwrap().stance,
        Stance::Restricts
    );

    let removed = store.delete_mark(&target, &agent, at(4)).unwrap().unwrap();
    assert_eq!(removed.stance, Stance::Restricts);
    assert_eq!(store.mark(&target).unwrap(), None);
    assert_eq!(store.delete_mark(&target, &agent, at(5)).unwrap(), None);

    // Unsetting nothing deletes nothing and records nothing.
    let history = store.mark_history(&target).unwrap();
    let actions: Vec<_> = history.iter().map(|event| event.action.clone()).collect();
    assert_eq!(
        actions,
        [MarkAction::Set, MarkAction::Set, MarkAction::Unset]
    );
    assert_eq!(history[2].agent, agent);
    assert_eq!(history[2].mark.as_ref().unwrap().stance, Stance::Restricts);
}

#[test]
fn unset_of_a_never_marked_target_writes_no_history() {
    let mut store = Store::memory().unwrap();
    let target: MarkTarget = "acme".parse().unwrap();
    let agent: AgentId = "agent-b".parse().unwrap();
    assert_eq!(store.delete_mark(&target, &agent, at(1)).unwrap(), None);
    assert!(store.mark_history(&target).unwrap().is_empty());
}

#[test]
fn due_marks_lists_expired_without_deleting() {
    let mut store = Store::memory().unwrap();
    store
        .put_mark(&mark("old/repo", Stance::Allows, at(1), 14))
        .unwrap();
    store
        .put_mark(&mark("new/repo", Stance::Bans, at(1), 90))
        .unwrap();

    let now = at(1) + TimeDelta::days(20);
    let due = store.due_marks(now).unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].target.key(), "old/repo");
    assert!(due[0].review_due(now));
    assert!(store.mark(&"old/repo".parse().unwrap()).unwrap().is_some());
}

#[test]
fn marks_lists_every_repo_and_org_mark_sorted_by_target() {
    let mut store = Store::memory().unwrap();
    assert!(store.marks().unwrap().is_empty());

    store
        .put_mark(&mark("zeta/repo", Stance::Allows, at(1), 14))
        .unwrap();
    store
        .put_mark(&mark("acme/widget", Stance::Bans, at(2), 90))
        .unwrap();
    store
        .put_mark(&mark("acme", Stance::Restricts, at(3), 30))
        .unwrap();
    store
        .put_mark(&mark("Beta/Thing", Stance::NoneFound, at(4), 14))
        .unwrap();

    let marks = store.marks().unwrap();
    let targets: Vec<String> = marks.iter().map(|m| m.target.key()).collect();
    assert_eq!(targets, ["acme", "acme/widget", "Beta/Thing", "zeta/repo"]);
    assert_eq!(marks[0].target.kind(), "org");
    assert_eq!(marks[1].target.kind(), "repo");
    assert_eq!(marks[1].stance, Stance::Bans);
    // Listing reads; it never deletes or rewrites.
    assert_eq!(store.marks().unwrap(), marks);

    let target: MarkTarget = "acme".parse().unwrap();
    let agent: AgentId = "agent-b".parse().unwrap();
    store.delete_mark(&target, &agent, at(5)).unwrap();
    assert_eq!(store.marks().unwrap().len(), 3);
}
