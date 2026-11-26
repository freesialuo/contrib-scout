//! Repository profiles and activity, FixtureTransport only (§7, §13).

use chrono::{SecondsFormat, TimeDelta, Utc};
use contrib_scout_core::config::Config;
use contrib_scout_core::gateway::{Budget, Gateway, HostPolicy};
use contrib_scout_core::profile::{compute_activity, GitHubProfiler, RepoProfiler};
use contrib_scout_core::store::Store;
use contrib_scout_core::transport::{ApiResponse, FixtureTransport};
use contrib_scout_core::{Freshness, RepoName, ScoutError};
use serde_json::{json, Value};
use std::sync::Arc;

fn ago(days: i64, hours: i64) -> String {
    (Utc::now() - TimeDelta::days(days) + TimeDelta::hours(hours))
        .to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn setup() -> (Arc<FixtureTransport>, GitHubProfiler) {
    let fixture = Arc::new(FixtureTransport::new());
    let store = Store::memory().unwrap().shared();
    let gateway = Gateway::new(fixture.clone(), HostPolicy::default(), Budget::new(20))
        .with_cache(store.clone())
        .with_sleep(Arc::new(|_| {}));
    let profiler = GitHubProfiler::new(Arc::new(gateway), store, Config::default());
    (fixture, profiler)
}

fn repo_json() -> Value {
    json!({
        "full_name": "acme/widget",
        "description": "Widgets",
        "archived": false,
        "fork": false,
        "license": {"spdx_id": "MIT", "name": "MIT License"},
        "default_branch": "main",
        "language": "Rust",
        "topics": ["cli", "widgets"],
        "stargazers_count": 42,
        "pushed_at": ago(1, 0),
    })
}

fn activity_json() -> Value {
    json!({"data": {"repository": {
        "pullRequests": {"nodes": [
            {"authorAssociation": "CONTRIBUTOR", "merged": true, "createdAt": ago(10, 0),
             "mergedAt": ago(9, 0), "author": {"login": "outsider"},
             "comments": {"nodes": [{"authorAssociation": "MEMBER", "createdAt": ago(10, 2), "author": {"login": "maint"}}]},
             "reviews": {"nodes": []}},
            {"authorAssociation": "NONE", "merged": false, "createdAt": ago(20, 0),
             "mergedAt": null, "author": {"login": "drive-by"},
             "comments": {"nodes": [{"authorAssociation": "NONE", "createdAt": ago(20, 1), "author": {"login": "drive-by"}}]},
             "reviews": {"nodes": [{"authorAssociation": "OWNER", "createdAt": ago(19, 0), "author": {"login": "boss"}}]}},
            {"authorAssociation": "MEMBER", "merged": true, "createdAt": ago(3, 0),
             "mergedAt": ago(2, 0), "author": {"login": "maint"},
             "comments": {"nodes": []}, "reviews": {"nodes": []}}
        ]},
        "issues": {"nodes": [
            {"authorAssociation": "NONE", "createdAt": ago(5, 0), "author": {"login": "user"},
             "comments": {"nodes": [{"authorAssociation": "COLLABORATOR", "createdAt": ago(5, 4), "author": {"login": "helper"}}]}}
        ]}
    }}})
}

#[test]
fn profile_is_cached_for_the_ttl_and_revalidated_with_etag() {
    let (fixture, profiler) = setup();
    let repo: RepoName = "acme/widget".parse().unwrap();
    fixture.expect_get(
        "/repos/acme/widget",
        ApiResponse::json(200, &repo_json()).with_etag("W/\"r1\""),
    );
    let profile = profiler.profile(&repo, Freshness::Default).unwrap();
    assert_eq!(profile.license.as_deref(), Some("MIT"));
    assert_eq!(profile.stars, 42);
    assert_eq!(profile.topics, ["cli", "widgets"]);
    assert!(!profile.archived);
    assert_eq!(fixture.request_count(), 1);

    // Within the TTL: zero requests.
    let again = profiler.profile(&repo, Freshness::Default).unwrap();
    assert_eq!(again.stars, 42);
    assert_eq!(fixture.request_count(), 1);

    // --fresh: conditional request; a 304 keeps the cached body.
    fixture.expect_get("/repos/acme/widget", ApiResponse::not_modified());
    let fresh = profiler.profile(&repo, Freshness::Fresh).unwrap();
    assert_eq!(fresh.stars, 42);
    let requests = fixture.requests();
    let last = requests.last().unwrap();
    match last {
        contrib_scout_core::transport::fixture::Recorded::Get(request) => {
            assert_eq!(request.if_none_match.as_deref(), Some("W/\"r1\""));
        }
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(profiler.gateway.metrics().served_304, 1);
    fixture.assert_drained();
}

#[test]
fn profile_live_always_hits_the_network_and_404_is_not_found() {
    let (fixture, profiler) = setup();
    let repo: RepoName = "acme/widget".parse().unwrap();
    fixture.expect_get("/repos/acme/widget", ApiResponse::json(200, &repo_json()));
    profiler.profile(&repo, Freshness::Default).unwrap();
    let mut archived = repo_json();
    archived["archived"] = json!(true);
    fixture.expect_get("/repos/acme/widget", ApiResponse::json(200, &archived));
    let live = profiler
        .profile_live(&repo, Freshness::Live.live().unwrap())
        .unwrap();
    assert!(live.get().archived);
    assert_eq!(fixture.request_count(), 2);

    let missing: RepoName = "acme/gone".parse().unwrap();
    fixture.expect_get(
        "/repos/acme/gone",
        ApiResponse::json(404, &json!({"message": "Not Found"})),
    );
    let error = profiler.profile(&missing, Freshness::Default).unwrap_err();
    assert!(matches!(
        ScoutError::find(&error),
        Some(ScoutError::NotFound { .. })
    ));
    fixture.assert_drained();
}

#[test]
fn offline_profile_without_cache_is_not_found() {
    let (fixture, profiler) = setup();
    let repo: RepoName = "acme/widget".parse().unwrap();
    let error = profiler.profile(&repo, Freshness::Offline).unwrap_err();
    assert!(matches!(
        ScoutError::find(&error),
        Some(ScoutError::NotFound { .. })
    ));
    let error = profiler.activity(&repo, Freshness::Offline).unwrap_err();
    assert!(matches!(
        ScoutError::find(&error),
        Some(ScoutError::NotFound { .. })
    ));
    assert_eq!(fixture.request_count(), 0);
}

#[test]
fn activity_is_one_graphql_query_and_cached() {
    let (fixture, profiler) = setup();
    let repo: RepoName = "acme/widget".parse().unwrap();
    fixture.expect_graphql(ApiResponse::json(200, &activity_json()));
    let activity = profiler.activity(&repo, Freshness::Default).unwrap();
    assert_eq!(activity.external_pr_merge_rate, Some(0.5));
    assert_eq!(activity.sample_size, 2);
    // Delays: PR 2h, PR review 1d, issue 4h → median 4h.
    assert_eq!(activity.median_first_response_secs, Some(4 * 3600));
    let last = activity.maintainer_last_active.unwrap();
    assert!((Utc::now() - TimeDelta::days(2) - last).num_seconds().abs() < 5);
    assert_eq!(fixture.request_count(), 1);

    let again = profiler.activity(&repo, Freshness::Default).unwrap();
    assert_eq!(again.sample_size, 2);
    assert_eq!(fixture.request_count(), 1);

    // The profile carries cached activity.
    fixture.expect_get("/repos/acme/widget", ApiResponse::json(200, &repo_json()));
    let profile = profiler.profile(&repo, Freshness::Default).unwrap();
    assert_eq!(profile.activity.unwrap().sample_size, 2);
    fixture.assert_drained();
}

#[test]
fn graphql_errors_are_surfaced() {
    let (fixture, profiler) = setup();
    let repo: RepoName = "acme/widget".parse().unwrap();
    fixture.expect_graphql(ApiResponse::json(
        200,
        &json!({"data": {"repository": null},
                "errors": [{"type": "FORBIDDEN", "message": "Resource not accessible by integration"}]}),
    ));
    let error = profiler.activity(&repo, Freshness::Default).unwrap_err();
    assert!(
        format!("{error:#}").contains("Resource not accessible by integration"),
        "{error:#}"
    );

    fixture.expect_graphql(ApiResponse::json(
        200,
        &json!({"data": {"repository": null},
                "errors": [{"type": "NOT_FOUND", "message": "Could not resolve to a Repository"}]}),
    ));
    let error = profiler.activity(&repo, Freshness::Default).unwrap_err();
    assert!(matches!(
        ScoutError::find(&error),
        Some(ScoutError::NotFound { .. })
    ));
    fixture.assert_drained();
}

#[test]
fn activity_with_no_external_prs_has_no_merge_rate() {
    let activity = compute_activity(
        &json!({"pullRequests": {"nodes": []}, "issues": {"nodes": []}}),
        Utc::now(),
    );
    assert_eq!(activity.external_pr_merge_rate, None);
    assert_eq!(activity.median_first_response_secs, None);
    assert_eq!(activity.maintainer_last_active, None);
    assert_eq!(activity.sample_size, 0);
}
