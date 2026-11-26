use contrib_scout_core::config::{parse_duration, Config};
use contrib_scout_core::coordinator::Resource;
use contrib_scout_core::envelope::{Envelope, ErrorInfo, Meta, SCHEMA};
use contrib_scout_core::gateway::{classify, Budget, Disposition};
use contrib_scout_core::models::Stance;
use contrib_scout_core::transport::fixture::Recorded;
use contrib_scout_core::transport::{ApiRequest, ApiResponse, FixtureTransport, Transport};
use contrib_scout_core::{not_implemented, ScoutError};
use serde_json::{json, Value};
use std::time::Duration;

#[test]
fn config_defaults_follow_the_design() {
    let config = Config::default();
    config.validate().unwrap();
    let day = Duration::from_secs(86_400);
    assert_eq!(config.mark_ttl(&Stance::Bans), 90 * day);
    assert_eq!(config.mark_ttl(&Stance::Restricts), 30 * day);
    assert_eq!(config.mark_ttl(&Stance::Disclose), 30 * day);
    assert_eq!(config.mark_ttl(&Stance::Welcomes), 14 * day);
    assert_eq!(config.mark_ttl(&Stance::NoneFound), 14 * day);
    assert_eq!(config.claim_lease(), 2 * day);
    let ttls = config.ttls();
    assert_eq!(ttls.issue_index, Duration::from_secs(3600));
    assert_eq!(ttls.issue_detail, Duration::from_secs(1800));
    assert_eq!(ttls.search, day);
    assert_eq!(ttls.repo, 7 * day);
    assert_eq!(config.reserves().get(Resource::Core), 500);
    assert_eq!(config.reserves().get(Resource::Search), 5);
}

#[test]
fn config_get_set() {
    let mut config = Config::default();
    config.set("claim_lease", "24h").unwrap();
    assert_eq!(config.claim_lease(), Duration::from_secs(86_400));
    config.set("reserve_core", "800").unwrap();
    assert_eq!(
        config.get("reserve_core").unwrap(),
        toml::Value::Integer(800)
    );
    config.set("transport", "http").unwrap();
    for (key, value) in [
        ("claim_lease", "soon"),
        ("reserve_core", "x"),
        ("nope", "1"),
        ("transport", "carrier"),
    ] {
        let error = config.set(key, value).unwrap_err();
        assert_eq!(
            ScoutError::find(&error).map(ScoutError::code),
            Some("usage"),
            "{key}={value}"
        );
    }
    assert!(parse_duration("7d").is_ok() && parse_duration("7").is_err());
}

#[test]
fn error_envelope() {
    let envelope = Envelope::failure(Meta::default(), &not_implemented("discover"));
    let value = serde_json::to_value(&envelope).unwrap();
    assert_eq!(value["schema"], SCHEMA);
    assert_eq!(value["data"], Value::Null);
    assert_eq!(value["error"]["code"], "not_implemented");
    assert_eq!(value["error"]["exit_code"], 1);
    assert_eq!(value["meta"]["reminders"], json!([]));
    let usage = ErrorInfo::from_error(&ScoutError::Usage("bad".into()).into());
    assert_eq!((usage.code.as_str(), usage.exit_code), ("usage", 2));
    let plain = ErrorInfo::from_error(&anyhow::anyhow!("boom"));
    assert_eq!((plain.code.as_str(), plain.exit_code), ("error", 1));
    let back: Envelope<Value> = serde_json::from_value(value).unwrap();
    assert_eq!(back.error.unwrap().code, "not_implemented");
}

#[test]
fn budget_and_classification() {
    let budget = Budget::new(2);
    budget.try_spend().unwrap();
    budget.try_spend().unwrap();
    assert_eq!(
        budget.try_spend().unwrap_err(),
        ScoutError::BudgetExhausted { limit: 2 }
    );
    assert_eq!(budget.remaining(), 0);
    assert_eq!(classify(&ApiResponse::new(200, "")), Disposition::Success);
    assert_eq!(
        classify(&ApiResponse::not_modified()),
        Disposition::NotModified
    );
    assert_eq!(classify(&ApiResponse::new(502, "")), Disposition::Retryable);
    assert_eq!(
        classify(&ApiResponse::new(403, "").header("Retry-After", "5")),
        Disposition::Retryable
    );
    assert_eq!(classify(&ApiResponse::new(404, "")), Disposition::Fatal);
}

#[test]
fn fixture_transport_scripts_and_records() {
    let fixture = FixtureTransport::new();
    fixture
        .expect_get(
            "/repos/o/r",
            ApiResponse::json(200, &json!({"name": "r"})).with_etag("W/\"abc\""),
        )
        .expect_get("/repos/o/r", ApiResponse::not_modified())
        .expect_graphql(ApiResponse::json(200, &json!({"data": {}})))
        .expect_network_down("/repos/o/r/issues/1");

    let first = fixture.get(&ApiRequest::get("/repos/o/r")).unwrap();
    assert_eq!(first.etag(), Some("W/\"abc\""));
    assert_eq!(first.json_body::<Value>().unwrap()["name"], "r");
    let second = fixture
        .get(&ApiRequest::get("/repos/o/r").etag(first.etag().map(String::from)))
        .unwrap();
    assert!(second.is_not_modified());
    fixture
        .graphql("query { viewer { login } }", &json!({}))
        .unwrap();
    let down = fixture
        .get(&ApiRequest::get("/repos/o/r/issues/1"))
        .unwrap_err();
    assert_eq!(
        ScoutError::find(&down).map(ScoutError::code),
        Some("network_unavailable")
    );
    fixture.assert_drained();

    let unexpected = fixture.get(&ApiRequest::get("/rate_limit")).unwrap_err();
    assert!(unexpected.to_string().contains("unexpected"));
    let requests = fixture.requests();
    assert_eq!(requests.len(), 5);
    match &requests[1] {
        Recorded::Get(request) => assert_eq!(request.if_none_match.as_deref(), Some("W/\"abc\"")),
        other => panic!("unexpected {other:?}"),
    }
    assert!(matches!(requests[2], Recorded::Graphql { .. }));

    let mismatched = FixtureTransport::new();
    mismatched.expect_get("/a", ApiResponse::new(200, ""));
    assert!(mismatched.get(&ApiRequest::get("/b")).is_err());
}
