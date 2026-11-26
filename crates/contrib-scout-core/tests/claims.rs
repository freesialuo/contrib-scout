//! Claim service: acquire, renew, release, list, expiry (§9).

use contrib_scout_core::claims::ClaimService;
use contrib_scout_core::config::Config;
use contrib_scout_core::coordinator::SqliteCoordinator;
use contrib_scout_core::models::{ClaimOutcome, ClaimStatus};
use contrib_scout_core::store::{SharedStore, Store};
use contrib_scout_core::{AgentId, IssueRef, ScoutError};
use std::sync::Arc;
use std::time::Duration;

fn agent(id: &str) -> AgentId {
    id.parse().unwrap()
}

fn issue(number: u64) -> IssueRef {
    format!("o/r#{number}").parse().unwrap()
}

fn service(store: &SharedStore, lease: Duration) -> ClaimService {
    let coordinator = Arc::new(SqliteCoordinator::from_config(
        store.clone(),
        agent("a1"),
        &Config::default(),
    ));
    ClaimService::new(coordinator, store.clone(), lease)
}

const DAY: Duration = Duration::from_secs(86_400);

#[test]
fn acquire_renew_release_and_list() {
    let store = Store::memory().unwrap().shared();
    let claims = service(&store, DAY);
    let a1 = agent("a1");
    let a2 = agent("a2");

    let ClaimOutcome::Acquired { claim } = claims.acquire(&issue(1), &a1).unwrap() else {
        panic!("expected a new claim")
    };
    assert_eq!(claim.status, ClaimStatus::Active);
    assert!(matches!(
        claims.acquire(&issue(1), &a1).unwrap(),
        ClaimOutcome::Renewed { .. }
    ));
    let ClaimOutcome::HeldByOther { claim: held } = claims.acquire(&issue(1), &a2).unwrap() else {
        panic!("a2 must not take a1's claim")
    };
    assert_eq!(held.agent, a1);
    claims.acquire(&issue(2), &a2).unwrap();

    assert_eq!(claims.list(None).unwrap().len(), 2);
    let mine = claims.list(Some(&a1)).unwrap();
    assert_eq!(mine.len(), 1);
    assert_eq!(mine[0].issue, issue(1));
    assert_eq!(claims.held_by(&issue(1), &a1).unwrap().unwrap().agent, a1);
    assert!(claims.held_by(&issue(1), &a2).unwrap().is_none());

    let renewed = claims.renew(&issue(1), &a1).unwrap();
    assert_eq!(renewed.claimed_at, claim.claimed_at);
    assert!(renewed.lease_until >= claim.lease_until);
    let with_branch = claims.record_branch(&issue(1), &a1, "fix-1").unwrap();
    assert_eq!(with_branch.branch.as_deref(), Some("fix-1"));
    assert_eq!(
        claims
            .held_by(&issue(1), &a1)
            .unwrap()
            .unwrap()
            .branch
            .as_deref(),
        Some("fix-1")
    );

    // Renewing someone else's claim, or none, is NotFound.
    for (number, who) in [(1, &a2), (3, &a1)] {
        let error = claims.renew(&issue(number), who).unwrap_err();
        assert!(matches!(
            ScoutError::find(&error),
            Some(ScoutError::NotFound { .. })
        ));
    }

    assert!(!claims.release(&issue(1), &a2).unwrap());
    assert!(claims.release(&issue(1), &a1).unwrap());
    assert!(!claims.release(&issue(1), &a1).unwrap());
    assert!(claims.active(&issue(1)).unwrap().is_none());
    // Released issues can be claimed by another agent.
    assert!(matches!(
        claims.acquire(&issue(1), &a2).unwrap(),
        ClaimOutcome::Acquired { .. }
    ));
}

#[test]
fn expired_leases_are_released() {
    let store = Store::memory().unwrap().shared();
    let short = service(&store, Duration::ZERO);
    short.acquire(&issue(1), &agent("a1")).unwrap();
    assert!(short.list(None).unwrap().is_empty());
    assert!(short.held_by(&issue(1), &agent("a1")).unwrap().is_none());
    assert!(short.renew(&issue(1), &agent("a1")).is_err());

    let normal = service(&store, DAY);
    assert!(matches!(
        normal.acquire(&issue(1), &agent("a2")).unwrap(),
        ClaimOutcome::Acquired { .. }
    ));
}
