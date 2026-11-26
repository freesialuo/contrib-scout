//! Discovery pipeline and local candidates, FixtureTransport only (§5, §5b, §7, §13).

use anyhow::anyhow;
use chrono::{SecondsFormat, TimeDelta, Utc};
use contrib_scout_core::config::Config;
use contrib_scout_core::discover::{
    search_path, search_query, CandidateQuery, DiscoverQuery, Discovery, Pipeline, Stage,
};
use contrib_scout_core::gateway::{Budget, Gateway, HostPolicy};
use contrib_scout_core::index::{IssueIndexer, SyncReport};
use contrib_scout_core::models::{
    Evidence, IssueDetail, IssueRef, PolicyMark, PolicyScope, ReminderKind, Stance,
};
use contrib_scout_core::store::{SharedStore, Store};
use contrib_scout_core::transport::fixture::Recorded;
use contrib_scout_core::transport::{ApiResponse, FixtureTransport};
use contrib_scout_core::{Freshness, Live, LiveRead, RepoName};
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// Records sync calls; the issue rows are inserted by the test.
#[derive(Default)]
struct FakeIndexer {
    synced: Mutex<Vec<String>>,
}

impl IssueIndexer for FakeIndexer {
    fn sync(&self, repo: &RepoName, _mode: Freshness) -> anyhow::Result<SyncReport> {
        self.synced.lock().unwrap().push(repo.to_string());
        Ok(SyncReport::new(repo))
    }

    fn issue(&self, _issue: &IssueRef, _mode: Freshness) -> anyhow::Result<IssueDetail> {
        Err(anyhow!("not used"))
    }

    fn issue_live(&self, _issue: &IssueRef, _live: LiveRead) -> anyhow::Result<Live<IssueDetail>> {
        Err(anyhow!("not used"))
    }
}

struct Harness {
    fixture: Arc<FixtureTransport>,
    store: SharedStore,
    indexer: Arc<FakeIndexer>,
    pipeline: Pipeline,
    db: PathBuf,
}

fn harness(name: &str, budget: u32) -> Harness {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("discover-{name}-{}-{unique}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("scout.db");
    let store = Store::open(&db).unwrap().shared();
    let fixture = Arc::new(FixtureTransport::new());
    let gateway = Gateway::new(fixture.clone(), HostPolicy::default(), Budget::new(budget))
        .with_cache(store.clone())
        .with_sleep(Arc::new(|_| {}));
    let indexer = Arc::new(FakeIndexer::default());
    let pipeline = Pipeline::new(Arc::new(gateway), store.clone(), Config::default())
        .with_indexer(indexer.clone());
    Harness {
        fixture,
        store,
        indexer,
        pipeline,
        db,
    }
}

fn ago(days: i64) -> String {
    (Utc::now() - TimeDelta::days(days)).to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn ts(days: i64) -> i64 {
    (Utc::now() - TimeDelta::days(days)).timestamp()
}

fn mark(target: &str, stance: Stance) -> PolicyMark {
    let now = Utc::now();
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
        note: "no AI PRs".into(),
        marked_by: "agent-a".parse().unwrap(),
        marked_at: now,
        expires_at: now + TimeDelta::days(90),
    }
}

fn repo_json(name: &str, archived: bool) -> Value {
    json!({
        "full_name": name, "archived": archived, "fork": false,
        "license": {"spdx_id": "Apache-2.0"}, "default_branch": "main",
        "language": "Rust", "topics": ["cli"], "stargazers_count": 10,
        "pushed_at": ago(2),
    })
}

fn activity_json() -> Value {
    json!({"data": {"repository": {
        "pullRequests": {"nodes": [
            {"authorAssociation": "CONTRIBUTOR", "merged": true, "createdAt": ago(10),
             "mergedAt": ago(9), "author": {"login": "outsider"},
             "comments": {"nodes": []}, "reviews": {"nodes": []}}
        ]},
        "issues": {"nodes": []}
    }}})
}

fn search_json(repos: &[&str]) -> Value {
    let items: Vec<Value> = repos
        .iter()
        .enumerate()
        .map(|(i, repo)| {
            json!({"number": i + 1, "repository_url": format!("https://api.github.com/repos/{repo}")})
        })
        .collect();
    json!({"total_count": items.len(), "incomplete_results": false, "items": items})
}

#[allow(clippy::too_many_arguments)]
fn insert_issue(
    db: &PathBuf,
    repo: &str,
    number: i64,
    state: &str,
    labels: &[&str],
    assignees: &[&str],
    comments: i64,
    raw: Value,
) {
    let conn = Connection::open(db).unwrap();
    conn.execute(
        "INSERT INTO issues (repo, number, title, state, labels_json, assignees_json, comments,
             created_at, updated_at, raw_json, fetched_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            repo,
            number,
            format!("issue {number}"),
            state,
            serde_json::to_string(labels).unwrap(),
            serde_json::to_string(assignees).unwrap(),
            comments,
            ts(20),
            ts(3),
            raw.to_string(),
            ts(0),
        ],
    )
    .unwrap();
}

fn insert_issue_mark(
    db: &PathBuf,
    repo: &str,
    number: i64,
    verdict: &str,
    seen_comments: i64,
    expires_at: Option<i64>,
) {
    let conn = Connection::open(db).unwrap();
    conn.execute(
        "INSERT INTO issue_marks (issue_ref, repo, number, verdict, reason, note, seen_updated_at,
             seen_comments, marked_by, marked_at, expires_at)
         VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?6, ?7, 'agent-a', ?8, ?9)",
        params![
            format!("{repo}#{number}"),
            repo,
            number,
            verdict,
            format!("{verdict} note"),
            ts(3),
            seen_comments,
            ts(1),
            expires_at,
        ],
    )
    .unwrap();
}

fn query() -> DiscoverQuery {
    DiscoverQuery {
        language: Some("rust".into()),
        limit: 10,
        ..DiscoverQuery::default()
    }
}

fn paths(fixture: &FixtureTransport) -> Vec<String> {
    fixture
        .requests()
        .iter()
        .map(|r| match r {
            Recorded::Get(request) => request.path.clone(),
            Recorded::Graphql { variables, .. } => format!("graphql {}", variables["name"]),
        })
        .collect()
}

#[test]
fn search_query_has_default_label_and_language() {
    let q = search_query(&query());
    assert_eq!(
        q,
        "is:issue is:open no:assignee archived:false label:\"good first issue\" language:rust"
    );
    let path = search_path(&q);
    assert!(
        path.starts_with("/search/issues?q=is%3Aissue%20is%3Aopen"),
        "{path}"
    );
    assert!(
        path.ends_with("&sort=updated&order=desc&per_page=100"),
        "{path}"
    );
}

/// Cold run: search, then per repo in search order. `b/banned` costs zero
/// requests; `c/archived` is dropped at metadata before any activity query.
fn script_cold_run(h: &Harness) {
    let q = query();
    h.fixture.expect_get(
        search_path(&search_query(&q)),
        ApiResponse::json(
            200,
            &search_json(&["a/one", "b/banned", "c/archived", "a/one"]),
        ),
    );
    h.fixture.expect_get(
        "/repos/a/one",
        ApiResponse::json(200, &repo_json("a/one", false)).with_etag("\"e1\""),
    );
    h.fixture
        .expect_graphql(ApiResponse::json(200, &activity_json()));
    h.fixture.expect_get(
        "/repos/c/archived",
        ApiResponse::json(200, &repo_json("c/archived", true)),
    );
}

#[test]
fn discover_cold_then_warm_within_ttl_makes_zero_requests() {
    let h = harness("warm", 50);
    h.store
        .lock()
        .unwrap()
        .put_mark(&mark("b/banned", Stance::Bans))
        .unwrap();
    insert_issue(
        &h.db,
        "a/one",
        1,
        "open",
        &["good first issue"],
        &[],
        0,
        json!({}),
    );
    insert_issue(&h.db, "a/one", 2, "open", &["bug"], &[], 0, json!({}));
    script_cold_run(&h);

    let report = h.pipeline.discover(&query(), Freshness::Default).unwrap();
    h.fixture.assert_drained();
    assert_eq!(h.fixture.request_count(), 4);
    let cold = paths(&h.fixture);
    assert!(cold.iter().all(|p| !p.contains("banned")), "{cold:?}");
    assert_eq!(
        cold.iter().filter(|p| p.starts_with("graphql")).count(),
        1,
        "activity only for a/one: {cold:?}"
    );

    assert_eq!(report.repos.len(), 1);
    let kept = &report.repos[0];
    assert_eq!(kept.profile.repo.to_string(), "a/one");
    assert!(kept.profile.activity.is_some());
    assert_eq!(kept.candidates.len(), 1, "only the labelled issue");
    assert_eq!(kept.candidates[0].issue.issue.number, 1);
    assert_eq!(report.excluded_marked, 1);
    let stages: Vec<(String, Stage)> = report
        .excluded
        .iter()
        .map(|e| (e.repo.to_string(), e.stage))
        .collect();
    assert_eq!(
        stages,
        [
            ("b/banned".to_owned(), Stage::PolicyMark),
            ("c/archived".to_owned(), Stage::Metadata)
        ]
    );
    // Every touched repo gets a reminder; marked and unmarked.
    let kinds: Vec<(String, ReminderKind)> = report
        .reminders
        .iter()
        .map(|r| (r.repo.to_string(), r.kind.clone()))
        .collect();
    assert_eq!(
        kinds,
        [
            ("a/one".to_owned(), ReminderKind::Unmarked),
            ("b/banned".to_owned(), ReminderKind::Marked),
            ("c/archived".to_owned(), ReminderKind::Unmarked)
        ]
    );
    assert_eq!(*h.indexer.synced.lock().unwrap(), ["a/one"]);

    // Warm: everything within its TTL.
    let warm = h.pipeline.discover(&query(), Freshness::Default).unwrap();
    assert_eq!(
        h.fixture.request_count(),
        4,
        "warm run must not send requests"
    );
    assert_eq!(warm.repos.len(), 1);
    assert_eq!(warm.excluded_marked, 1);
    assert_eq!(warm.new_repos, 0);
    assert_eq!(h.pipeline.gateway.metrics().requests, 4);
}

#[test]
fn include_marked_keeps_bans_repo_without_activity() {
    let h = harness("include", 50);
    h.store
        .lock()
        .unwrap()
        .put_mark(&mark("b", Stance::Bans))
        .unwrap();
    insert_issue(
        &h.db,
        "b/banned",
        1,
        "open",
        &["good first issue"],
        &[],
        0,
        json!({}),
    );
    let q = DiscoverQuery {
        include_marked: true,
        ..query()
    };
    h.fixture.expect_get(
        search_path(&search_query(&q)),
        ApiResponse::json(200, &search_json(&["b/banned"])),
    );
    h.fixture.expect_get(
        "/repos/b/banned",
        ApiResponse::json(200, &repo_json("b/banned", false)),
    );
    let report = h.pipeline.discover(&q, Freshness::Default).unwrap();
    h.fixture.assert_drained();
    assert_eq!(report.excluded_marked, 0);
    assert_eq!(report.repos.len(), 1);
    assert!(
        report.repos[0].profile.activity.is_none(),
        "no activity for Bans"
    );
    assert_eq!(report.reminders[0].kind, ReminderKind::Marked);
    assert_eq!(
        report.reminders[0].target.as_ref().unwrap().to_string(),
        "b"
    );
}

#[test]
fn graphql_errors_surface_as_warnings_in_discover() {
    let h = harness("gqlerr", 50);
    insert_issue(
        &h.db,
        "a/one",
        1,
        "open",
        &["good first issue"],
        &[],
        0,
        json!({}),
    );
    h.fixture.expect_get(
        search_path(&search_query(&query())),
        ApiResponse::json(200, &search_json(&["a/one"])),
    );
    h.fixture.expect_get(
        "/repos/a/one",
        ApiResponse::json(200, &repo_json("a/one", false)),
    );
    h.fixture.expect_graphql(ApiResponse::json(
        200,
        &json!({"data": null, "errors": [{"type": "RATE_LIMITED", "message": "API rate limit exceeded"}]}),
    ));
    let report = h.pipeline.discover(&query(), Freshness::Default).unwrap();
    h.fixture.assert_drained();
    assert!(report.partial);
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("API rate limit exceeded")),
        "{:?}",
        report.warnings
    );
    // The repo stays, scored without activity.
    assert_eq!(report.repos.len(), 1);
    assert!(report.repos[0].profile.activity.is_none());
}

#[test]
fn budget_exhaustion_stops_the_run_with_partial_results() {
    let h = harness("budget", 2);
    h.fixture.expect_get(
        search_path(&search_query(&query())),
        ApiResponse::json(200, &search_json(&["a/one", "c/two"])),
    );
    h.fixture.expect_get(
        "/repos/a/one",
        ApiResponse::json(200, &repo_json("a/one", false)),
    );
    let report = h.pipeline.discover(&query(), Freshness::Default).unwrap();
    h.fixture.assert_drained();
    assert!(report.partial);
    assert!(report
        .warnings
        .iter()
        .any(|w| w.contains("stopped at a/one")));
    assert_eq!(report.reminders.len(), 1, "c/two was never touched");
}

#[test]
fn explain_makes_no_requests() {
    let h = harness("explain", 50);
    let cold = h.pipeline.plan(&query(), Freshness::Default).unwrap();
    assert_eq!(h.fixture.request_count(), 0);
    assert!(!cold.search_cached);
    assert_eq!(cold.estimated_requests, 1 + 3 * 10);
    h.store
        .lock()
        .unwrap()
        .put_mark(&mark("b/banned", Stance::Bans))
        .unwrap();

    script_cold_run(&h);
    h.pipeline.discover(&query(), Freshness::Default).unwrap();
    let before = h.fixture.request_count();
    let warm = h.pipeline.plan(&query(), Freshness::Default).unwrap();
    assert_eq!(h.fixture.request_count(), before);
    assert!(warm.search_cached);
    assert_eq!(warm.known_repos.len(), 3);
    // b/banned is excluded, c/archived fails metadata from cache; only a/one's index sync remains.
    assert_eq!(warm.estimated_requests, 1);
    assert_eq!(warm.reminders.len(), 3);

    let offline = h.pipeline.plan(&query(), Freshness::Offline).unwrap();
    assert_eq!(offline.estimated_requests, 0);
}

#[test]
fn candidates_are_local_and_honor_issue_and_policy_marks() {
    let h = harness("candidates", 50);
    {
        let mut store = h.store.lock().unwrap();
        store.put_mark(&mark("b/banned", Stance::Bans)).unwrap();
    }
    let gfi = &["good first issue"][..];
    insert_issue(&h.db, "a/one", 1, "open", gfi, &[], 0, json!({}));
    insert_issue(&h.db, "a/one", 2, "open", gfi, &[], 0, json!({}));
    insert_issue(&h.db, "a/one", 3, "open", &["bug"], &[], 5, json!({}));
    insert_issue(&h.db, "b/banned", 4, "open", gfi, &[], 0, json!({}));
    insert_issue(&h.db, "a/one", 5, "open", gfi, &["someone"], 0, json!({}));
    insert_issue(&h.db, "a/one", 6, "closed", gfi, &[], 0, json!({}));
    insert_issue(
        &h.db,
        "a/one",
        7,
        "open",
        gfi,
        &[],
        0,
        json!({"pull_request": {"url": "x"}}),
    );
    insert_issue(&h.db, "a/one", 8, "open", gfi, &[], 0, json!({}));
    insert_issue_mark(&h.db, "a/one", 2, "skip", 0, None);
    // Shortlisted with a stale snapshot (5 comments now, 2 when marked).
    insert_issue_mark(&h.db, "a/one", 3, "shortlist", 2, None);
    // An expired skip mark no longer applies.
    insert_issue_mark(&h.db, "a/one", 8, "skip", 0, Some(ts(1)));

    let report = h
        .pipeline
        .candidates(&CandidateQuery {
            limit: 20,
            ..CandidateQuery::default()
        })
        .unwrap();
    assert_eq!(
        h.fixture.request_count(),
        0,
        "candidates makes zero requests"
    );
    let numbers: Vec<u64> = report
        .candidates
        .iter()
        .map(|c| c.issue.issue.number)
        .collect();
    assert_eq!(numbers[0], 3, "shortlist ranks first: {numbers:?}");
    let mut rest = numbers[1..].to_vec();
    rest.sort_unstable();
    assert_eq!(rest, [1, 8]);
    let shortlisted = report.candidates[0].mark.as_ref().unwrap();
    assert!(shortlisted.changed_since_mark);
    assert_eq!(shortlisted.note, "shortlist note");
    assert_eq!(report.excluded_skipped, 1);
    assert_eq!(report.excluded_marked, 1);
    let reminded: Vec<String> = report
        .reminders
        .iter()
        .map(|r| r.repo.to_string())
        .collect();
    assert_eq!(reminded, ["a/one", "b/banned"]);

    // min_score drops low scores but never the shortlist.
    let strict = h
        .pipeline
        .candidates(&CandidateQuery {
            min_score: Some(99.0),
            limit: 20,
            ..CandidateQuery::default()
        })
        .unwrap();
    let numbers: Vec<u64> = strict
        .candidates
        .iter()
        .map(|c| c.issue.issue.number)
        .collect();
    assert_eq!(numbers, [3]);

    let all = h
        .pipeline
        .candidates(&CandidateQuery {
            limit: 20,
            include_marked: true,
            ..CandidateQuery::default()
        })
        .unwrap();
    let mut numbers: Vec<u64> = all
        .candidates
        .iter()
        .map(|c| c.issue.issue.number)
        .collect();
    numbers.sort_unstable();
    assert_eq!(numbers, [1, 2, 3, 4, 8]);
    assert_eq!(all.excluded_marked, 0);
    assert_eq!(all.excluded_skipped, 0);
    assert_eq!(h.fixture.request_count(), 0);
}

#[test]
fn candidates_language_filter_uses_cached_profiles() {
    let h = harness("lang", 50);
    insert_issue(
        &h.db,
        "a/one",
        1,
        "open",
        &["good first issue"],
        &[],
        0,
        json!({}),
    );
    insert_issue(
        &h.db,
        "z/other",
        2,
        "open",
        &["good first issue"],
        &[],
        0,
        json!({}),
    );
    h.fixture.expect_get(
        "/repos/a/one",
        ApiResponse::json(200, &repo_json("a/one", false)),
    );
    use contrib_scout_core::profile::RepoProfiler;
    h.pipeline
        .profiler
        .profile(&"a/one".parse().unwrap(), Freshness::Default)
        .unwrap();
    let report = h
        .pipeline
        .candidates(&CandidateQuery {
            language: Some("rust".into()),
            limit: 5,
            ..CandidateQuery::default()
        })
        .unwrap();
    let numbers: Vec<u64> = report
        .candidates
        .iter()
        .map(|c| c.issue.issue.number)
        .collect();
    assert_eq!(numbers, [1]);
}
