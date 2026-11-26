//! Cross-handle coordination on one database file: windows, single-flight,
//! claims (§9, §13). Separate `Store` handles stand in for separate processes
//! (each has its own SQLite connection and lock-file handle).

use chrono::{TimeDelta, Utc};
use contrib_scout_core::config::{QuotaReserves, WindowLimits};
use contrib_scout_core::coordinator::{CacheKey, Coordinator, Resource, SqliteCoordinator};
use contrib_scout_core::freshness::Origin;
use contrib_scout_core::gateway::{Budget, Gateway, HostPolicy};
use contrib_scout_core::models::{AgentId, ClaimOutcome, ClaimStatus, IssueRef};
use contrib_scout_core::store::Store;
use contrib_scout_core::transport::{ApiRequest, ApiResponse, FixtureTransport, Transport};
use contrib_scout_core::{Freshness, ScoutError};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

fn temp_db(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!(
            "{name}-{}-{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap()
        ))
        .join("scout.db")
}

fn agent(name: &str) -> AgentId {
    name.parse().unwrap()
}

fn coordinator(path: &PathBuf, who: &str, windows: WindowLimits) -> SqliteCoordinator {
    SqliteCoordinator::new(
        Store::open(path).unwrap().shared(),
        agent(who),
        QuotaReserves {
            core: 500,
            search: 5,
            graphql: 500,
        },
        windows,
    )
}

#[test]
fn window_limit_is_shared_across_handles() {
    let path = temp_db("window");
    let windows = WindowLimits {
        per_minute: 3,
        per_hour: 100,
        search_per_minute: 1,
    };
    let a = coordinator(&path, "agent-a", windows);
    let b = coordinator(&path, "agent-b", windows);
    a.reserve(Resource::Core, 2).unwrap();
    b.reserve(Resource::Core, 1).unwrap();
    let error = a.reserve(Resource::Core, 1).unwrap_err();
    assert!(matches!(
        ScoutError::find(&error),
        Some(ScoutError::WindowLimit {
            window: "minute",
            limit: 3,
            ..
        })
    ));
    let error = b.reserve(Resource::Core, 1).unwrap_err();
    assert_eq!(ScoutError::find(&error).unwrap().code(), "window_limit");

    let store = a.store.lock().unwrap();
    let since = Utc::now() - TimeDelta::minutes(1);
    assert_eq!(store.requests_since(None, since).unwrap(), 3);
}

#[test]
fn search_has_its_own_minute_window() {
    let path = temp_db("search-window");
    let windows = WindowLimits {
        per_minute: 100,
        per_hour: 100,
        search_per_minute: 1,
    };
    let a = coordinator(&path, "agent-a", windows);
    let b = coordinator(&path, "agent-b", windows);
    a.reserve(Resource::Search, 1).unwrap();
    assert!(b.reserve(Resource::Search, 1).is_err());
    assert!(b.reserve(Resource::Core, 1).is_ok());
}

#[test]
fn gateway_waits_once_for_a_full_minute_window_then_fails() {
    let path = temp_db("window-wait");
    let windows = WindowLimits {
        per_minute: 1,
        per_hour: 100,
        search_per_minute: 1,
    };
    let fixture = Arc::new(FixtureTransport::new());
    let sleeps = Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = Arc::clone(&sleeps);
    let gateway = Gateway::new(fixture.clone(), HostPolicy::default(), Budget::new(10))
        .with_coordinator(Arc::new(coordinator(&path, "agent-a", windows)))
        .with_sleep(Arc::new(move |d| log.lock().unwrap().push(d)));
    fixture.expect_any_get(ApiResponse::new(200, "{}"));
    gateway.send(&ApiRequest::get("/repos/o/r")).unwrap();
    let error = gateway.send(&ApiRequest::get("/repos/o/r")).unwrap_err();
    assert_eq!(ScoutError::find(&error).unwrap().code(), "window_limit");
    assert_eq!(sleeps.lock().unwrap().as_slice(), &[gateway.window_wait]);
    assert_eq!(fixture.request_count(), 1);
}

#[test]
fn quota_is_visible_across_handles() {
    let path = temp_db("quota");
    let windows = WindowLimits {
        per_minute: 100,
        per_hour: 100,
        search_per_minute: 100,
    };
    let a = coordinator(&path, "agent-a", windows);
    let b = coordinator(&path, "agent-b", windows);
    let now = Utc::now();
    a.observe(&contrib_scout_core::coordinator::QuotaSnapshot {
        resource: Resource::Search,
        limit: 30,
        remaining: 5,
        reset_at: now + TimeDelta::seconds(40),
        observed_at: now,
    })
    .unwrap();
    let error = b.reserve(Resource::Search, 1).unwrap_err();
    assert_eq!(ScoutError::find(&error).unwrap().code(), "quota_reserved");
    assert!(b.reserve(Resource::Core, 1).is_ok());
}

#[test]
fn in_flight_requests_count_against_the_reserve() {
    let path = temp_db("in-flight");
    let windows = WindowLimits {
        per_minute: 100,
        per_hour: 100,
        search_per_minute: 100,
    };
    let a = coordinator(&path, "agent-a", windows);
    let b = coordinator(&path, "agent-b", windows);
    let now = Utc::now();
    // 503 left, reserve 500: room for three requests.
    a.observe(&contrib_scout_core::coordinator::QuotaSnapshot {
        resource: Resource::Core,
        limit: 5000,
        remaining: 503,
        reset_at: now + TimeDelta::minutes(30),
        observed_at: now,
    })
    .unwrap();
    // Agent a has two requests in flight (reserved, not yet answered).
    let pending = a.reserve(Resource::Core, 2).unwrap();
    // b may take the last one, then the reserve is reached.
    let last = b.reserve(Resource::Core, 1).unwrap();
    let error = b.reserve(Resource::Core, 1).unwrap_err();
    assert_eq!(ScoutError::find(&error).unwrap().code(), "quota_reserved");
    // Answered requests are no longer counted: their responses carry the
    // newer `x-ratelimit-*` snapshot.
    a.settle(&pending, Some(200)).unwrap();
    b.settle(&last, Some(200)).unwrap();
    assert!(b.reserve(Resource::Core, 1).is_ok());
}

/// Wraps the fixture with a delay so concurrent fetches really overlap.
struct Slow(Arc<FixtureTransport>);

impl Transport for Slow {
    fn name(&self) -> &'static str {
        "slow-fixture"
    }

    fn get(&self, request: &ApiRequest) -> contrib_scout_core::Result<ApiResponse> {
        thread::sleep(Duration::from_millis(300));
        self.0.get(request)
    }

    fn graphql(&self, query: &str, variables: &Value) -> contrib_scout_core::Result<ApiResponse> {
        self.0.graphql(query, variables)
    }
}

fn single_flight_race(freshness: Freshness) {
    let path = temp_db("flight");
    let windows = WindowLimits {
        per_minute: 100,
        per_hour: 100,
        search_per_minute: 100,
    };
    // Exactly one scripted answer: a second network request would fail the test.
    let fixture = Arc::new(FixtureTransport::new());
    fixture.expect_get(
        "/repos/o/r",
        ApiResponse::new(200, "{\"n\":1}").with_etag("\"a\""),
    );
    let transport: Arc<dyn Transport> = Arc::new(Slow(Arc::clone(&fixture)));
    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = ["agent-a", "agent-b"]
        .into_iter()
        .map(|who| {
            let coordinator = coordinator(&path, who, windows);
            let store = Arc::clone(&coordinator.store);
            let gateway = Gateway::new(
                Arc::clone(&transport),
                HostPolicy::default(),
                Budget::new(5),
            )
            .with_coordinator(Arc::new(coordinator))
            .with_cache(store)
            .with_sleep(Arc::new(|_| {}));
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                gateway
                    .get(
                        &ApiRequest::get("/repos/o/r"),
                        freshness,
                        Duration::from_secs(3600),
                    )
                    .unwrap()
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(fixture.request_count(), 1, "single-flight must dedupe");
    let mut origins: Vec<_> = results.iter().map(|f| f.origin).collect();
    origins.sort_by_key(|o| format!("{o:?}"));
    assert_eq!(origins, vec![Origin::Cache, Origin::Network]);
    assert!(results.iter().all(|f| f.response.body == b"{\"n\":1}"));
}

#[test]
fn single_flight_dedupes_default_reads_across_handles() {
    single_flight_race(Freshness::Default);
}

#[test]
fn single_flight_dedupes_fresh_reads_across_handles() {
    single_flight_race(Freshness::Fresh);
}

#[test]
fn single_flight_times_out_while_another_handle_holds_the_key() {
    let path = temp_db("flight-timeout");
    let windows = WindowLimits {
        per_minute: 100,
        per_hour: 100,
        search_per_minute: 100,
    };
    let a = coordinator(&path, "agent-a", windows);
    let b = coordinator(&path, "agent-b", windows);
    let key = CacheKey::new("GET /repos/o/r");
    let held = a.single_flight(&key, Duration::ZERO).unwrap();
    assert!(held.is_some());
    assert!(b
        .single_flight(&key, Duration::from_millis(60))
        .unwrap()
        .is_none());
    drop(held);
    assert!(b.single_flight(&key, Duration::ZERO).unwrap().is_some());
}

#[test]
fn claims_acquire_renew_conflict_expire_release() {
    let path = temp_db("claims");
    let windows = WindowLimits {
        per_minute: 100,
        per_hour: 100,
        search_per_minute: 100,
    };
    let a = coordinator(&path, "agent-a", windows);
    let b = coordinator(&path, "agent-b", windows);
    let issue: IssueRef = "owner/repo#7".parse().unwrap();
    let lease = Duration::from_secs(48 * 3600);

    let ClaimOutcome::Acquired { claim } = a.claim(&issue, &agent("agent-a"), lease).unwrap()
    else {
        panic!("expected Acquired");
    };
    assert_eq!(claim.status, ClaimStatus::Active);
    assert!(claim.lease_until > Utc::now() + TimeDelta::hours(47));

    assert!(matches!(
        a.claim(&issue, &agent("agent-a"), lease).unwrap(),
        ClaimOutcome::Renewed { .. }
    ));
    let ClaimOutcome::HeldByOther { claim } = b.claim(&issue, &agent("agent-b"), lease).unwrap()
    else {
        panic!("expected HeldByOther");
    };
    assert_eq!(claim.agent, agent("agent-a"));

    // B releasing A's claim is a no-op; A releasing frees it for B.
    b.release(&issue, &agent("agent-b")).unwrap();
    assert!(matches!(
        b.claim(&issue, &agent("agent-b"), lease).unwrap(),
        ClaimOutcome::HeldByOther { .. }
    ));
    a.release(&issue, &agent("agent-a")).unwrap();
    assert!(matches!(
        b.claim(&issue, &agent("agent-b"), lease).unwrap(),
        ClaimOutcome::Acquired { .. }
    ));

    // Expiry: B's lease ends; A can take it, and B's row is marked expired.
    let mut store = b.store.lock().unwrap();
    let later = Utc::now() + TimeDelta::hours(49);
    assert!(store.active_claim(&issue, later).unwrap().is_none());
    let outcome = store
        .claim_issue(
            &issue,
            &agent("agent-a"),
            later,
            later + TimeDelta::hours(48),
            Some("fix-7"),
        )
        .unwrap();
    let ClaimOutcome::Acquired { claim } = outcome else {
        panic!("expected Acquired after expiry, got {outcome:?}");
    };
    assert_eq!(claim.branch.as_deref(), Some("fix-7"));
    assert!(store
        .claims(Some(&agent("agent-b")), later)
        .unwrap()
        .is_empty());
    assert_eq!(store.claims(None, later).unwrap().len(), 1);
    assert_eq!(
        store.expire_claims(later + TimeDelta::hours(100)).unwrap(),
        1
    );
    assert!(!store
        .release_claim(&issue, &agent("agent-a"), later + TimeDelta::hours(100))
        .unwrap());
}
