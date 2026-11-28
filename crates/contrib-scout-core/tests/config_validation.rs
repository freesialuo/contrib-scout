//! Config validation edge cases: zero claim lease, signed durations.

use contrib_scout_core::config::{parse_duration, Config};
use std::time::Duration;

#[test]
fn a_zero_claim_lease_is_rejected() {
    for zero in ["0s", "0m", "0h", "0d", "0w"] {
        let config = Config {
            claim_lease: zero.into(),
            ..Config::default()
        };
        let error = config.validate().unwrap_err();
        assert!(format!("{error:#}").contains("claim_lease"), "{error:#}");
        assert!(
            Config::default().set("claim_lease", zero).is_err(),
            "{zero}"
        );
    }
    let mut config = Config::default();
    config.set("claim_lease", "1s").unwrap();
    assert_eq!(config.claim_lease(), Duration::from_secs(1));
}

#[test]
fn zero_mark_ttls_stay_allowed() {
    let mut config = Config::default();
    config.set("mark_ttl_allows", "0s").unwrap();
    config.set("policy_files_ttl", "0s").unwrap();
    config.validate().unwrap();
}

#[test]
fn durations_reject_a_sign() {
    assert!(parse_duration("+5m").is_err());
    assert!(parse_duration("-5m").is_err());
    assert!(parse_duration("+0s").is_err());
    assert_eq!(parse_duration("5m").unwrap(), Duration::from_secs(300));
    assert_eq!(parse_duration("0s").unwrap(), Duration::ZERO);
}
