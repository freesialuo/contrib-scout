//! Gateway pipeline and conditional cache, FixtureTransport only (§13).

use chrono::{TimeDelta, Utc};
use contrib_scout_core::config::{QuotaReserves, WindowLimits};
use contrib_scout_core::coordinator::{CacheKey, Resource, SqliteCoordinator};
use contrib_scout_core::freshness::Origin;
use contrib_scout_core::gateway::{Budget, Gateway, HostPolicy, SleepFn};
use contrib_scout_core::store::{CachedResponse, SharedStore, Store};
use contrib_scout_core::transport::fixture::Recorded;
use contrib_scout_core::transport::{ApiRequest, ApiResponse, FixtureTransport};
use contrib_scout_core::{Freshness, ScoutError};
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const HOUR: Duration = Duration::from_secs(3600);

struct Harness {
    fixture: Arc<FixtureTransport>,
    store: SharedStore,
    sleeps: Arc<Mutex<Vec<Duration>>>,
    gateway: Gateway,
}

fn recorder() -> (SleepFn, Arc<Mutex<Vec<Duration>>>) {
    let sleeps = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&sleeps);
    (Arc::new(move |d| log.lock().unwrap().push(d)), sleeps)
}

fn windows() -> WindowLimits {
    WindowLimits {
        per_minute: 1000,
        per_hour: 10_000,
        search_per_minute: 100,
    }
}

fn reserves() -> QuotaReserves {
    QuotaReserves {
        core: 500,
        search: 5,
        graphql: 500,
    }
}

fn harness_with(budget: u32, store: Store) -> Harness {
    let fixture = Arc::new(FixtureTransport::new());
    let store = store.shared();
    let coordinator = SqliteCoordinator::new(
        Arc::clone(&store),
        "agent-a".parse().unwrap(),
        reserves(),
        windows(),
    );
    let (sleep, sleeps) = recorder();
    let gateway = Gateway::new(fixture.clone(), HostPolicy::default(), Budget::new(budget))
        .with_coordinator(Arc::new(coordinator))
        .with_cache(Arc::clone(&store))
        .with_sleep(sleep);
    Harness {
        fixture,
        store,
        sleeps,
        gateway,
    }
}

fn harness(budget: u32) -> Harness {
    harness_with(budget, Store::memory().unwrap())
}

fn ok_body(text: &str) -> ApiResponse {
    ApiResponse::json(200, &json!({ "text": text })).with_etag("W/\"v1\"")
}

#[test]
fn budget_exhaustion_counts_every_attempt() {
    let h = harness(2);
    h.fixture.expect_any_get(ApiResponse::new(503, "down"));
    h.fixture.expect_any_get(ApiResponse::new(503, "down"));
    let error = h.gateway.send(&ApiRequest::get("/repos/o/r")).unwrap_err();
    assert!(matches!(
        ScoutError::find(&error),
        Some(ScoutError::BudgetExhausted { limit: 2 })
    ));
    assert_eq!(h.fixture.request_count(), 2);
    assert_eq!(h.gateway.metrics().requests, 2);
    assert_eq!(h.gateway.metrics().retries, 1);
    let error = h.gateway.send(&ApiRequest::get("/repos/o/r")).unwrap_err();
    assert_eq!(ScoutError::find(&error).unwrap().code(), "budget_exhausted");
    assert_eq!(h.fixture.request_count(), 2);
}

#[test]
fn retries_503_with_jittered_backoff_then_succeeds() {
    let h = harness(10);
    h.fixture
        .expect_get("/repos/o/r", ApiResponse::new(503, "down"));
    h.fixture
        .expect_get("/repos/o/r", ApiResponse::new(502, "down"));
    h.fixture.expect_get("/repos/o/r", ok_body("hi"));
    let response = h.gateway.send(&ApiRequest::get("/repos/o/r")).unwrap();
    assert_eq!(response.status, 200);
    h.fixture.assert_drained();
    let metrics = h.gateway.metrics();
    assert_eq!((metrics.requests, metrics.retries), (3, 2));
    let policy = HostPolicy::default();
    let backoffs: Vec<_> = h.sleeps.lock().unwrap().clone();
    // Two backoffs (burst covers the token bucket), each under its full-jitter ceiling.
    assert_eq!(backoffs.len(), 2, "{backoffs:?}");
    assert!(backoffs[0] <= policy.retry_base);
    assert!(backoffs[1] <= policy.retry_base * 2);
}

#[test]
fn gives_up_after_three_attempts_and_fatal_4xx_is_not_retried() {
    let h = harness(10);
    for _ in 0..3 {
        h.fixture.expect_any_get(ApiResponse::new(500, "boom"));
    }
    let error = h.gateway.send(&ApiRequest::get("/repos/o/r")).unwrap_err();
    assert!(matches!(
        ScoutError::find(&error),
        Some(ScoutError::HttpStatus { status: 500, .. })
    ));
    assert_eq!(h.fixture.request_count(), 3);

    h.fixture.expect_any_get(ApiResponse::new(422, "bad"));
    let error = h.gateway.send(&ApiRequest::get("/repos/o/r")).unwrap_err();
    assert!(matches!(
        ScoutError::find(&error),
        Some(ScoutError::HttpStatus { status: 422, .. })
    ));
    assert_eq!(h.fixture.request_count(), 4);

    // 404 is a normal answer ("absent"), not an error.
    h.fixture.expect_any_get(ApiResponse::new(404, "{}"));
    assert_eq!(
        h.gateway
            .send(&ApiRequest::get("/repos/o/x"))
            .unwrap()
            .status,
        404
    );
}

#[test]
fn retry_after_is_honored_instead_of_backoff() {
    let h = harness(10);
    h.fixture
        .expect_any_get(ApiResponse::new(429, "slow down").header("retry-after", "7"));
    h.fixture.expect_any_get(ok_body("ok"));
    assert_eq!(
        h.gateway
            .send(&ApiRequest::get("/repos/o/r"))
            .unwrap()
            .status,
        200
    );
    let sleeps = h.sleeps.lock().unwrap().clone();
    assert_eq!(sleeps.len(), 1, "{sleeps:?}");
    assert!(sleeps[0] > Duration::from_secs(6) && sleeps[0] <= Duration::from_secs(7));

    // Secondary limit: 403 with retry-after is retryable.
    h.fixture
        .expect_any_get(ApiResponse::new(403, "secondary").header("retry-after", "1"));
    h.fixture.expect_any_get(ok_body("ok"));
    assert_eq!(
        h.gateway
            .send(&ApiRequest::get("/repos/o/r"))
            .unwrap()
            .status,
        200
    );

    // A wait beyond max_retry_wait fails immediately.
    h.fixture
        .expect_any_get(ApiResponse::new(429, "later").header("retry-after", "3600"));
    let error = h.gateway.send(&ApiRequest::get("/repos/o/r")).unwrap_err();
    assert!(matches!(
        ScoutError::find(&error),
        Some(ScoutError::HttpStatus { status: 429, .. })
    ));
    h.fixture.assert_drained();
}

#[test]
fn not_modified_serves_cached_body_and_counts_304() {
    let h = harness(10);
    let request = ApiRequest::get("/repos/o/r");
    h.fixture.expect_get("/repos/o/r", ok_body("cached"));
    let first = h.gateway.get(&request, Freshness::Default, HOUR).unwrap();
    assert_eq!(first.origin, Origin::Network);

    // Within TTL: no request.
    let second = h.gateway.get(&request, Freshness::Default, HOUR).unwrap();
    assert_eq!(second.origin, Origin::Cache);
    assert_eq!(h.fixture.request_count(), 1);

    // Fresh: conditional request with the weak ETag verbatim; 304 → cached body.
    h.fixture
        .expect_get("/repos/o/r", ApiResponse::not_modified());
    let third = h.gateway.get(&request, Freshness::Fresh, HOUR).unwrap();
    assert_eq!(third.origin, Origin::NotModified);
    assert_eq!(third.response.status, 200);
    assert_eq!(third.response.body, first.response.body);
    let Recorded::Get(sent) = &h.fixture.requests()[1] else {
        panic!("expected a GET");
    };
    assert_eq!(sent.if_none_match.as_deref(), Some("W/\"v1\""));
    assert_eq!(h.gateway.metrics().served_304, 1);
}

#[test]
fn expired_ttl_revalidates_and_bumps_fetched_at() {
    let h = harness(10);
    let key = CacheKey::for_request(&ApiRequest::get("/repos/o/r"));
    let old = Utc::now() - TimeDelta::hours(5);
    h.store
        .lock()
        .unwrap()
        .save_response(
            &CachedResponse {
                key: key.as_str().to_owned(),
                status: 200,
                etag: Some("\"e\"".into()),
                last_modified: None,
                headers: vec![],
                body: b"old".to_vec(),
            },
            old,
        )
        .unwrap();
    h.fixture.expect_any_get(ApiResponse::not_modified());
    let fetched = h
        .gateway
        .get(&ApiRequest::get("/repos/o/r"), Freshness::Default, HOUR)
        .unwrap();
    assert_eq!(fetched.origin, Origin::NotModified);
    assert_eq!(fetched.response.body, b"old");
    let stored = h
        .store
        .lock()
        .unwrap()
        .cached_response(key.as_str())
        .unwrap()
        .unwrap();
    assert!(stored.fetched_at > old + TimeDelta::hours(4));
}

#[test]
fn offline_with_empty_cache_errors_without_requests() {
    let h = harness(10);
    let error = h
        .gateway
        .get(&ApiRequest::get("/repos/o/r"), Freshness::Offline, HOUR)
        .unwrap_err();
    assert_eq!(ScoutError::find(&error).unwrap().code(), "not_found");
    assert_eq!(h.fixture.request_count(), 0);
}

#[test]
fn caches_404_but_never_5xx_or_403() {
    let h = harness(10);
    h.fixture.expect_any_get(ApiResponse::new(404, "{}"));
    let missing = h
        .gateway
        .get(&ApiRequest::get("/repos/o/gone"), Freshness::Default, HOUR)
        .unwrap();
    assert!(missing.is_not_found());
    let again = h
        .gateway
        .get(&ApiRequest::get("/repos/o/gone"), Freshness::Offline, HOUR)
        .unwrap();
    assert!(again.is_not_found());
    assert_eq!(again.origin, Origin::Cache);

    h.fixture.expect_any_get(ApiResponse::new(403, "forbidden"));
    assert!(h
        .gateway
        .get(
            &ApiRequest::get("/repos/o/secret"),
            Freshness::Default,
            HOUR
        )
        .is_err());
    for _ in 0..3 {
        h.fixture.expect_any_get(ApiResponse::new(503, "down"));
    }
    assert!(h
        .gateway
        .get(&ApiRequest::get("/repos/o/flaky"), Freshness::Default, HOUR)
        .is_err());
    let store = h.store.lock().unwrap();
    assert!(store
        .cached_response("GET /repos/o/secret")
        .unwrap()
        .is_none());
    assert!(store
        .cached_response("GET /repos/o/flaky")
        .unwrap()
        .is_none());
}

#[test]
fn default_falls_back_to_stale_cache_when_network_is_down() {
    let h = harness(10);
    let request = ApiRequest::get("/repos/o/r");
    h.fixture.expect_any_get(ok_body("v1"));
    h.gateway.get(&request, Freshness::Default, HOUR).unwrap();
    for _ in 0..3 {
        h.fixture.expect_network_down("/repos/o/r");
    }
    let fetched = h.gateway.get(&request, Freshness::Fresh, HOUR).unwrap();
    assert_eq!(fetched.origin, Origin::Cache);
    assert!(fetched.stale.is_some());
    h.fixture.assert_drained();
}

#[test]
fn live_never_uses_the_cache_when_network_is_down() {
    let h = harness(10);
    let request = ApiRequest::get("/repos/o/r/issues/1");
    h.fixture.expect_any_get(ok_body("issue"));
    h.gateway.get(&request, Freshness::Default, HOUR).unwrap();

    for _ in 0..3 {
        h.fixture.expect_network_down("/repos/o/r/issues/1");
    }
    let live = Freshness::Live.live().unwrap();
    let error = h.gateway.get_live(&request, live).unwrap_err();
    assert_eq!(
        ScoutError::find(&error).unwrap().code(),
        "network_unavailable"
    );
    // Same through `get` with Freshness::Live.
    for _ in 0..3 {
        h.fixture.expect_network_down("/repos/o/r/issues/1");
    }
    assert!(h.gateway.get(&request, Freshness::Live, HOUR).is_err());
    h.fixture.assert_drained();
}

#[test]
fn live_304_is_a_live_confirmation() {
    let h = harness(10);
    let request = ApiRequest::get("/repos/o/r/issues/1");
    h.fixture.expect_any_get(ok_body("issue"));
    let live = Freshness::Live.live().unwrap();
    let first = h.gateway.get_live(&request, live).unwrap();
    assert!(!first.not_modified());
    h.fixture.expect_any_get(ApiResponse::not_modified());
    let second = h.gateway.get_live(&request, live).unwrap();
    assert!(second.not_modified());
    assert_eq!(second.get().status, 200);
    assert_eq!(second.get().body, first.get().body);
    assert_eq!(h.fixture.request_count(), 2);
}

#[test]
fn quota_headers_are_recorded_and_reserve_refuses_under_threshold() {
    let h = harness(10);
    let reset = (Utc::now() + TimeDelta::minutes(30))
        .timestamp()
        .to_string();
    h.fixture.expect_any_get(
        ok_body("x")
            .header("x-ratelimit-limit", "5000")
            .header("x-ratelimit-remaining", "400")
            .header("x-ratelimit-reset", &reset)
            .header("x-ratelimit-resource", "core"),
    );
    h.gateway.send(&ApiRequest::get("/repos/o/r")).unwrap();
    let snapshot = h
        .store
        .lock()
        .unwrap()
        .quota(Resource::Core)
        .unwrap()
        .unwrap();
    assert_eq!((snapshot.limit, snapshot.remaining), (5000, 400));
    assert_eq!(snapshot.reset_at.timestamp().to_string(), reset);

    // 400 <= reserve 500: refused before any request, budget refunded.
    let used = h.gateway.budget.used();
    let error = h.gateway.send(&ApiRequest::get("/repos/o/r")).unwrap_err();
    assert!(matches!(
        ScoutError::find(&error),
        Some(ScoutError::QuotaReserved {
            resource: Resource::Core,
            remaining: 400,
            reserve: 500,
            ..
        })
    ));
    assert_eq!(h.fixture.request_count(), 1);
    assert_eq!(h.gateway.budget.used(), used);

    // Search is a separate bucket, inferred from the path.
    h.fixture.expect_any_get(ok_body("results"));
    h.gateway
        .send(&ApiRequest::get("/search/repositories?q=x"))
        .unwrap();
    let log = h
        .store
        .lock()
        .unwrap()
        .requests_since(Some(Resource::Search), Utc::now() - TimeDelta::minutes(1))
        .unwrap();
    assert_eq!(log, 1);
}

#[test]
fn reserve_ignores_snapshot_whose_reset_passed() {
    let h = harness(10);
    let coordinator = h.gateway.coordinator.clone().unwrap();
    let now = Utc::now();
    coordinator
        .observe(&contrib_scout_core::coordinator::QuotaSnapshot {
            resource: Resource::Core,
            limit: 5000,
            remaining: 0,
            reset_at: now - TimeDelta::minutes(1),
            observed_at: now - TimeDelta::minutes(61),
        })
        .unwrap();
    assert!(coordinator.reserve(Resource::Core, 1).is_ok());
}

#[test]
fn older_quota_observations_never_overwrite_newer() {
    let mut store = Store::memory().unwrap();
    let now = Utc::now();
    let snapshot = |remaining, observed| contrib_scout_core::coordinator::QuotaSnapshot {
        resource: Resource::Core,
        limit: 5000,
        remaining,
        reset_at: now + TimeDelta::minutes(30),
        observed_at: observed,
    };
    store.record_quota(&snapshot(100, now)).unwrap();
    store
        .record_quota(&snapshot(900, now - TimeDelta::seconds(10)))
        .unwrap();
    assert_eq!(store.quota(Resource::Core).unwrap().unwrap().remaining, 100);
    store
        .record_quota(&snapshot(90, now + TimeDelta::seconds(1)))
        .unwrap();
    assert_eq!(store.quota(Resource::Core).unwrap().unwrap().remaining, 90);
    assert_eq!(store.quotas().unwrap().len(), 1);
}

#[test]
fn graphql_goes_through_the_pipeline_without_retries() {
    let h = harness(10);
    h.fixture
        .expect_graphql(ApiResponse::new(502, "bad gateway"));
    assert!(h
        .gateway
        .graphql("{ viewer { login } }", &json!({}))
        .is_err());
    assert_eq!(h.fixture.request_count(), 1);
    h.fixture
        .expect_graphql(ApiResponse::json(200, &json!({"data": {}})));
    assert_eq!(
        h.gateway
            .graphql("{ viewer { login } }", &json!({}))
            .unwrap()
            .status,
        200
    );
    let graphql = h
        .store
        .lock()
        .unwrap()
        .requests_since(Some(Resource::Graphql), Utc::now() - TimeDelta::minutes(1))
        .unwrap();
    assert_eq!(graphql, 2);
}

#[test]
fn rejects_foreign_origins_without_spending_budget() {
    let h = harness(10);
    assert!(h
        .gateway
        .send(&ApiRequest::get("https://evil.example/repos/o/r"))
        .is_err());
    assert_eq!(h.gateway.budget.used(), 0);
    assert_eq!(h.fixture.request_count(), 0);
}

#[test]
fn cache_prune_and_stats() {
    let h = harness(10);
    h.fixture.expect_any_get(ok_body("x"));
    h.gateway
        .get(&ApiRequest::get("/repos/o/r"), Freshness::Default, HOUR)
        .unwrap();
    let mut store = h.store.lock().unwrap();
    let stats = store.stats().unwrap();
    assert_eq!(stats.rows["http_cache"], 1);
    assert_eq!(stats.rows["request_log"], 1);
    let pruned = store.prune(Utc::now() + TimeDelta::minutes(1)).unwrap();
    assert_eq!(pruned.deleted["http_cache"], 1);
    assert_eq!(pruned.deleted["request_log"], 1);
    assert!(!pruned.deleted.contains_key("policy_marks"));
    assert_eq!(store.stats().unwrap().rows["http_cache"], 0);
}

#[test]
fn transport_failures_do_not_stay_in_flight() {
    let h = harness(100);
    let coordinator = h.gateway.coordinator.clone().unwrap();
    let now = Utc::now();
    coordinator
        .observe(&contrib_scout_core::coordinator::QuotaSnapshot {
            resource: Resource::Core,
            limit: 5000,
            remaining: 503,
            reset_at: now + TimeDelta::minutes(30),
            observed_at: now - TimeDelta::seconds(1),
        })
        .unwrap();
    for _ in 0..2 {
        for _ in 0..3 {
            h.fixture.expect_network_down("/repos/o/r");
        }
        assert!(h.gateway.send(&ApiRequest::get("/repos/o/r")).is_err());
    }
    assert_eq!(h.fixture.request_count(), 6);
    // Failed requests may have reached GitHub: they still count in the windows.
    let logged = h
        .store
        .lock()
        .unwrap()
        .requests_since(None, Utc::now() - TimeDelta::minutes(1))
        .unwrap();
    assert_eq!(logged, 6);
    // But they are not "in flight": 503 remaining is still above the reserve.
    assert!(coordinator.reserve(Resource::Core, 1).is_ok());
}

#[test]
fn future_fetched_at_is_treated_as_expired() {
    let h = harness(10);
    let key = CacheKey::for_request(&ApiRequest::get("/repos/o/r"));
    let future = Utc::now() + TimeDelta::hours(5);
    h.store
        .lock()
        .unwrap()
        .save_response(
            &CachedResponse {
                key: key.as_str().to_owned(),
                status: 200,
                etag: Some("\"e\"".into()),
                last_modified: None,
                headers: vec![],
                body: b"skewed".to_vec(),
            },
            future,
        )
        .unwrap();
    h.fixture.expect_any_get(ApiResponse::not_modified());
    let fetched = h
        .gateway
        .get(&ApiRequest::get("/repos/o/r"), Freshness::Default, HOUR)
        .unwrap();
    assert_eq!(fetched.origin, Origin::NotModified);
    assert_eq!(h.fixture.request_count(), 1);
    // The 304 pulls `fetched_at` back to now.
    let stored = h
        .store
        .lock()
        .unwrap()
        .cached_response(key.as_str())
        .unwrap()
        .unwrap();
    assert!(stored.fetched_at <= Utc::now(), "{}", stored.fetched_at);
}
