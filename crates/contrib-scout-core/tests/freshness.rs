use contrib_scout_core::{Freshness, ScoutError};
use std::time::Duration;

const TTL: Duration = Duration::from_secs(3600);

#[test]
fn fetch_rules() {
    let young = Some(Duration::from_secs(60));
    let old = Some(TTL);
    assert!(!Freshness::Offline.allows_fetch());
    for mode in [Freshness::Default, Freshness::Fresh, Freshness::Live] {
        assert!(mode.allows_fetch());
    }
    assert!(!Freshness::Offline.needs_fetch(None, TTL));
    assert!(Freshness::Default.needs_fetch(None, TTL));
    assert!(!Freshness::Default.needs_fetch(young, TTL));
    assert!(Freshness::Default.needs_fetch(old, TTL));
    assert!(Freshness::Fresh.needs_fetch(young, TTL));
    assert!(Freshness::Live.needs_fetch(young, TTL));
    assert!(!Freshness::Live.may_fall_back_to_cache());
    assert!(Freshness::Default.may_fall_back_to_cache());
}

#[test]
fn only_live_produces_a_live_token() {
    for mode in [Freshness::Offline, Freshness::Default, Freshness::Fresh] {
        assert!(mode.live().is_none());
        let error = mode.require_live("gate start").unwrap_err();
        assert!(matches!(error, ScoutError::LiveRequired { .. }));
        assert_eq!(error.code(), "live_required");
    }
    let token = Freshness::Live.require_live("gate start").unwrap();
    assert_eq!(token.freshness(), Freshness::Live);
    assert_eq!(Freshness::from(token), Freshness::Live);
}
