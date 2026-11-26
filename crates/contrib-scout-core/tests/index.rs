//! Issue index sync and issue details, FixtureTransport only (§7, §13).

use chrono::{DateTime, TimeDelta, TimeZone, Utc};
use contrib_scout_core::config::Config;
use contrib_scout_core::gateway::{Budget, Gateway, HostPolicy};
use contrib_scout_core::index::{GitHubIndexer, IssueIndexer, SyncSkip};
use contrib_scout_core::store::{IssueFilter, SharedStore, Store, SyncState};
use contrib_scout_core::transport::fixture::Recorded;
use contrib_scout_core::transport::{ApiResponse, FixtureTransport};
use contrib_scout_core::{Freshness, IssueRef, RepoName, ScoutError};
use serde_json::{json, Value};
use std::sync::Arc;

const FIRST: &str = "/repos/o/r/issues?state=open&sort=updated&direction=asc&per_page=100";

struct Harness {
    fixture: Arc<FixtureTransport>,
    store: SharedStore,
    indexer: GitHubIndexer,
}

fn harness_on(store: SharedStore, budget: u32, config: Config) -> Harness {
    let fixture = Arc::new(FixtureTransport::new());
    let policy = HostPolicy {
        max_attempts: 1,
        ..HostPolicy::default()
    };
    let gateway = Gateway::new(fixture.clone(), policy, Budget::new(budget))
        .with_cache(store.clone())
        .with_sleep(Arc::new(|_| {}));
    let indexer = GitHubIndexer::new(Arc::new(gateway), store.clone(), config);
    Harness {
        fixture,
        store,
        indexer,
    }
}

fn harness(budget: u32) -> Harness {
    harness_on(Store::memory().unwrap().shared(), budget, Config::default())
}

fn repo() -> RepoName {
    "o/r".parse().unwrap()
}

fn issue_ref(number: u64) -> IssueRef {
    IssueRef::new(repo(), number)
}

fn at(hour: u32, minute: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 20, hour, minute, 0).unwrap()
}

fn iso(time: DateTime<Utc>) -> String {
    time.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn issue(number: u64, updated: DateTime<Utc>, state: &str, comments: u32) -> Value {
    json!({
        "number": number,
        "title": format!("Issue {number}"),
        "state": state,
        "labels": [{"name": "bug"}],
        "assignees": [],
        "assignee": null,
        "comments": comments,
        "created_at": "2026-11-01T00:00:00Z",
        "updated_at": iso(updated),
        "user": {"login": "reporter"},
        "body": format!("Body of {number}"),
        "html_url": format!("https://github.com/o/r/issues/{number}"),
    })
}

fn pull(number: u64, updated: DateTime<Utc>) -> Value {
    let mut value = issue(number, updated, "open", 0);
    value["pull_request"] = json!({"url": "https://api.github.com/repos/o/r/pulls/2"});
    value
}

fn page(items: Vec<Value>, next: Option<&str>) -> ApiResponse {
    let response = ApiResponse::json(200, &Value::Array(items));
    match next {
        Some(next) => response.header(
            "link",
            &format!("<{next}>; rel=\"next\", <{next}>; rel=\"last\""),
        ),
        None => response,
    }
}

fn paths(fixture: &FixtureTransport) -> Vec<String> {
    fixture
        .requests()
        .iter()
        .filter_map(Recorded::path)
        .map(str::to_owned)
        .collect()
}

#[test]
fn first_sync_paginates_skips_prs_and_stores() {
    let h = harness(10);
    let second = "https://api.github.com/repositories/1/issues?state=open&sort=updated&direction=asc&per_page=100&page=2";
    h.fixture.expect_get(
        FIRST,
        page(
            vec![issue(1, at(1, 0), "open", 0), pull(2, at(2, 0))],
            Some(second),
        ),
    );
    h.fixture
        .expect_get(second, page(vec![issue(3, at(3, 0), "open", 4)], None));

    let before = Utc::now();
    let report = h.indexer.sync(&repo(), Freshness::Default).unwrap();
    h.fixture.assert_drained();
    assert!(report.first_pass && !report.partial);
    assert_eq!(report.since, None);
    assert_eq!((report.new_issues, report.updated), (2, 2));
    assert_eq!(
        (report.skipped_prs, report.pages, report.requests),
        (1, 2, 2)
    );

    let store = h.store.lock().unwrap();
    let row = store.issue(&issue_ref(3)).unwrap().unwrap().data;
    assert_eq!(row.comments, 4);
    assert_eq!(row.labels, ["bug"]);
    assert!(
        store.issue(&issue_ref(2)).unwrap().is_none(),
        "PRs are not indexed"
    );
    let state = store.sync_state(&repo()).unwrap();
    assert!(!state.partial);
    assert!(state.last_full_at.is_some());
    // Watermark = when the pass started, not the newest updated_at.
    let since = state.since.unwrap();
    assert!(since >= before - TimeDelta::seconds(1) && since <= Utc::now());
    let open = store
        .query_issues(&IssueFilter {
            open_only: true,
            ..IssueFilter::default()
        })
        .unwrap();
    assert_eq!(open.len(), 2);
}

#[test]
fn second_sync_within_ttl_sends_nothing_and_offline_never_does() {
    let h = harness(10);
    h.fixture
        .expect_get(FIRST, page(vec![issue(1, at(1, 0), "open", 0)], None));
    h.indexer.sync(&repo(), Freshness::Default).unwrap();
    assert_eq!(h.fixture.request_count(), 1);

    let again = h.indexer.sync(&repo(), Freshness::Default).unwrap();
    assert_eq!(again.skipped, Some(SyncSkip::Fresh));
    assert_eq!((again.requests, again.updated), (0, 0));
    let offline = h.indexer.sync(&repo(), Freshness::Offline).unwrap();
    assert_eq!(offline.skipped, Some(SyncSkip::Offline));
    assert_eq!(h.fixture.request_count(), 1);
}

#[test]
fn incremental_sync_sends_since_with_overlap_and_upserts_changes() {
    let h = harness(10);
    let watermark = at(10, 0);
    {
        let mut store = h.store.lock().unwrap();
        let rows = [
            issue(1, at(1, 0), "open", 0),
            issue(5, at(9, 55), "open", 1),
        ];
        let rows: Vec<_> = rows
            .iter()
            .map(|raw| {
                (
                    contrib_scout_core::index::parse_issue_row(&repo(), raw, at(10, 0)).unwrap(),
                    raw.clone(),
                )
            })
            .collect();
        store.save_issues(&rows).unwrap();
        store
            .save_sync_state(
                &repo(),
                &SyncState {
                    since: Some(watermark),
                    last_full_at: Some(at(0, 0)),
                    last_sync_at: Some(at(10, 0)),
                    ..SyncState::default()
                },
            )
            .unwrap();
    }
    // Default overlap is 10 minutes; later passes list every state.
    let path = "/repos/o/r/issues?state=all&sort=updated&direction=asc&per_page=100&since=2026-11-20T09:50:00Z";
    h.fixture.expect_get(
        path,
        page(
            vec![
                issue(5, at(9, 55), "open", 1),    // overlap: unchanged
                issue(1, at(10, 30), "closed", 2), // closed since
                issue(7, at(10, 40), "open", 0),   // new
            ],
            None,
        ),
    );
    let report = h.indexer.sync(&repo(), Freshness::Default).unwrap();
    h.fixture.assert_drained();
    assert!(!report.first_pass);
    assert_eq!(report.since, Some(at(9, 50)));
    assert_eq!(
        (report.new_issues, report.updated, report.unchanged),
        (1, 2, 1)
    );
    assert!(report.has_changes());

    let store = h.store.lock().unwrap();
    let row = store.issue(&issue_ref(1)).unwrap().unwrap().data;
    assert_eq!(row.state.as_str(), "closed");
    assert_eq!(row.comments, 2);
    let state = store.sync_state(&repo()).unwrap();
    assert_eq!(state.last_full_at, Some(at(0, 0)));
    assert!(state.since.unwrap() > watermark);
}

#[test]
fn overlap_never_regresses_a_newer_row() {
    let h = harness(10);
    let mut store = h.store.lock().unwrap();
    let newer = issue(1, at(12, 0), "closed", 3);
    let older = issue(1, at(11, 0), "open", 1);
    let row = |raw: &Value| {
        (
            contrib_scout_core::index::parse_issue_row(&repo(), raw, Utc::now()).unwrap(),
            raw.clone(),
        )
    };
    assert_eq!(store.save_issues(&[row(&newer)]).unwrap().new, 1);
    assert_eq!(store.save_issues(&[row(&older)]).unwrap().unchanged, 1);
    let stored = store.issue(&issue_ref(1)).unwrap().unwrap().data;
    assert_eq!(stored.updated_at, at(12, 0));
}

#[test]
fn budget_exhaustion_is_partial_and_the_next_run_resumes() {
    let store = Store::memory().unwrap().shared();
    let h = harness_on(store.clone(), 1, Config::default());
    let second = "https://api.github.com/repositories/1/issues?state=open&page=2";
    h.fixture.expect_get(
        FIRST,
        page(
            vec![issue(1, at(1, 0), "open", 0), issue(2, at(2, 0), "open", 0)],
            Some(second),
        ),
    );
    let report = h.indexer.sync(&repo(), Freshness::Default).unwrap();
    assert!(report.partial);
    assert_eq!(report.new_issues, 2);
    assert_eq!(report.watermark, Some(at(2, 0)));
    assert!(
        report.warnings[0].contains("budget"),
        "{:?}",
        report.warnings
    );
    h.fixture.assert_drained();
    let state = store.lock().unwrap().sync_state(&repo()).unwrap();
    assert!(state.partial && state.last_full_at.is_none());
    let first_started = state.pass_started_at.unwrap();

    // A partial index ignores the TTL and resumes from the stored watermark.
    let h = harness_on(store.clone(), 10, Config::default());
    let resume = format!("{FIRST}&since=2026-11-20T01:50:00Z");
    h.fixture.expect_get(
        resume.as_str(),
        page(
            vec![issue(2, at(2, 0), "open", 0), issue(3, at(3, 0), "open", 0)],
            None,
        ),
    );
    let report = h.indexer.sync(&repo(), Freshness::Default).unwrap();
    h.fixture.assert_drained();
    assert!(!report.partial && report.first_pass);
    assert_eq!((report.new_issues, report.unchanged), (1, 1));
    let state = store.lock().unwrap().sync_state(&repo()).unwrap();
    assert!(!state.partial);
    assert!(state.last_full_at.is_some());
    // The completed pass's watermark is the start of its first (interrupted) run.
    assert_eq!(state.since, Some(first_started));
}

#[test]
fn unsafe_listing_link_is_refused() {
    let h = harness(10);
    h.fixture.expect_get(
        FIRST,
        page(
            vec![issue(1, at(1, 0), "open", 0)],
            Some("https://evil.example.com/repos/o/r/issues?page=2"),
        ),
    );
    let report = h.indexer.sync(&repo(), Freshness::Default).unwrap();
    assert!(report.partial);
    assert_eq!(h.fixture.request_count(), 1);
}

// ---------------------------------------------------------------------------
// Details
// ---------------------------------------------------------------------------

const ISSUE: &str = "/repos/o/r/issues/9";
const TIMELINE: &str = "/repos/o/r/issues/9/timeline?per_page=100";
const TIMELINE_TWO: &str = "https://api.github.com/repositories/1/issues/9/timeline?page=2";

fn comment(id: u32) -> Value {
    json!({
        "user": {"login": format!("user{id}")},
        "author_association": "NONE",
        "body": format!("comment {id}"),
        "created_at": "2026-11-02T00:00:00Z",
        "html_url": format!("https://github.com/o/r/issues/9#issuecomment-{id}"),
    })
}

fn cross_reference(number: u64, merged: bool) -> Value {
    json!({
        "event": "cross-referenced",
        "source": {"issue": {
            "number": number,
            "state": "open",
            "user": {"login": "someone"},
            "html_url": format!("https://github.com/o/r/pull/{number}"),
            "pull_request": {
                "html_url": format!("https://github.com/o/r/pull/{number}"),
                "merged_at": if merged { json!("2026-11-03T00:00:00Z") } else { Value::Null },
            },
        }},
    })
}

fn disconnected(number: u64) -> Value {
    json!({
        "event": "disconnected",
        "subject": {
            "html_url": format!("https://github.com/o/r/pull/{number}"),
            "pull_request": {"html_url": format!("https://github.com/o/r/pull/{number}")},
        },
    })
}

fn config_with_comments(n: u32) -> Config {
    Config {
        gate_comments: n,
        ..Config::default()
    }
}

/// Issue (3 comments, keep the last 2 → pages 1 and 2 of size 2) and a two-page timeline.
fn script_detail(fixture: &FixtureTransport, etags: bool) {
    let tag = |response: ApiResponse, etag: &str| {
        if etags {
            response.with_etag(etag)
        } else {
            response
        }
    };
    fixture.expect_get(
        ISSUE,
        tag(
            ApiResponse::json(200, &issue(9, at(5, 0), "open", 3)),
            "\"i\"",
        ),
    );
    fixture.expect_get(
        "/repos/o/r/issues/9/comments?per_page=2&page=1",
        tag(page(vec![comment(1), comment(2)], None), "\"c1\""),
    );
    fixture.expect_get(
        "/repos/o/r/issues/9/comments?per_page=2&page=2",
        tag(page(vec![comment(3)], None), "\"c2\""),
    );
    fixture.expect_get(
        TIMELINE,
        tag(
            page(
                vec![
                    json!({"event": "labeled"}),
                    cross_reference(10, false),
                    cross_reference(11, true),
                ],
                Some(TIMELINE_TWO),
            ),
            "\"t1\"",
        ),
    );
    fixture.expect_get(
        TIMELINE_TWO,
        tag(page(vec![disconnected(10)], None), "\"t2\""),
    );
}

#[test]
fn detail_reads_comments_and_timeline_prs_across_pages() {
    let h = harness_on(
        Store::memory().unwrap().shared(),
        10,
        config_with_comments(2),
    );
    script_detail(&h.fixture, false);
    let fetched = h
        .indexer
        .fetch_issue(&issue_ref(9), Freshness::Default)
        .unwrap();
    h.fixture.assert_drained();
    let detail = fetched.detail;
    assert_eq!(detail.body.as_deref(), Some("Body of 9"));
    assert_eq!(detail.author.as_deref(), Some("reporter"));
    let bodies: Vec<_> = detail.comments.iter().map(|c| c.body.as_str()).collect();
    assert_eq!(bodies, ["comment 2", "comment 3"]);
    assert_eq!(detail.linked_prs.len(), 1, "PR 10 was disconnected");
    let pr = &detail.linked_prs[0];
    assert_eq!(pr.url, "https://github.com/o/r/pull/11");
    assert_eq!(pr.event, "cross-referenced");
    assert_eq!(pr.state.as_deref(), Some("merged"));
    assert_eq!(pr.author.as_deref(), Some("someone"));
    assert!(fetched.warnings.is_empty());

    // The detail read refreshed the index row.
    let row = h.store.lock().unwrap().issue(&issue_ref(9)).unwrap();
    assert_eq!(row.unwrap().data.comments, 3);

    // Within issue_detail_ttl: zero requests.
    h.indexer.issue(&issue_ref(9), Freshness::Default).unwrap();
    assert_eq!(h.fixture.request_count(), 5);
}

#[test]
fn cross_origin_timeline_link_is_refused() {
    let h = harness_on(
        Store::memory().unwrap().shared(),
        10,
        config_with_comments(0),
    );
    h.fixture.expect_get(
        ISSUE,
        ApiResponse::json(200, &issue(9, at(5, 0), "open", 0)),
    );
    h.fixture.expect_get(
        TIMELINE,
        page(
            vec![cross_reference(10, false)],
            Some("https://example.com/repos/o/r/issues/9/timeline?page=2"),
        ),
    );
    let error = h
        .indexer
        .issue(&issue_ref(9), Freshness::Default)
        .unwrap_err();
    assert!(format!("{error:#}").contains("refusing"), "{error:#}");
    assert_eq!(h.fixture.request_count(), 2);
    assert!(!paths(&h.fixture).iter().any(|p| p.contains("example.com")));
}

#[test]
fn missing_issue_is_not_found() {
    let h = harness_on(
        Store::memory().unwrap().shared(),
        10,
        config_with_comments(0),
    );
    h.fixture.expect_get(
        ISSUE,
        ApiResponse::json(404, &json!({"message": "Not Found"})),
    );
    let error = h
        .indexer
        .issue(&issue_ref(9), Freshness::Default)
        .unwrap_err();
    assert!(matches!(
        ScoutError::find(&error),
        Some(ScoutError::NotFound { .. })
    ));
}

#[test]
fn issue_live_revalidates_and_never_falls_back_to_cache() {
    let h = harness_on(
        Store::memory().unwrap().shared(),
        20,
        config_with_comments(2),
    );
    script_detail(&h.fixture, true);
    h.indexer
        .fetch_issue(&issue_ref(9), Freshness::Default)
        .unwrap();

    // Everything unchanged: five 304s still count as live confirmation.
    for _ in 0..5 {
        h.fixture.expect_any_get(ApiResponse::not_modified());
    }
    let live = Freshness::Live.live().unwrap();
    let detail = h.indexer.issue_live(&issue_ref(9), live).unwrap();
    assert!(detail.not_modified());
    assert_eq!(detail.get().linked_prs.len(), 1);
    let revalidation = h.fixture.requests()[5..].to_vec();
    assert!(revalidation.iter().all(|r| match r {
        Recorded::Get(request) => request.if_none_match.is_some(),
        Recorded::Graphql { .. } => false,
    }));

    // Network down: an error, not the cached copy.
    h.fixture.expect_network_down(ISSUE);
    let error = h.indexer.issue_live(&issue_ref(9), live).unwrap_err();
    assert!(matches!(
        ScoutError::find(&error),
        Some(ScoutError::NetworkUnavailable { .. })
    ));
    // The cached read still works offline.
    assert!(h.indexer.issue(&issue_ref(9), Freshness::Offline).is_ok());
}
