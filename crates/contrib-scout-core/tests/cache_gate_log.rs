//! `gate_log` retention: pruned only on request, counted by `cache stats` (§6, §10).

use chrono::{DateTime, TimeDelta, TimeZone, Utc};
use contrib_scout_core::models::GateReport;
use contrib_scout_core::store::Store;
use serde_json::json;

fn at(day: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, day, 12, 0, 0).unwrap()
}

fn report(issue: &str, as_of: DateTime<Utc>) -> GateReport {
    serde_json::from_value(json!({
        "kind": "start",
        "issue": issue,
        "agent": "agent-a",
        "verdict": "pass",
        "raw": {},
        "as_of": as_of,
    }))
    .unwrap()
}

#[test]
fn plain_prune_never_touches_the_gate_log() {
    let mut store = Store::memory().unwrap();
    store.log_gate(&report("o/r#1", at(1))).unwrap();
    store.log_gate(&report("o/r#2", at(2))).unwrap();

    let pruned = store.prune(at(28)).unwrap();
    assert!(!pruned.deleted.contains_key("gate_log"), "{pruned:?}");
    assert_eq!(store.stats().unwrap().rows["gate_log"], 2);
}

#[test]
fn prune_gate_log_deletes_only_rows_before_the_cutoff() {
    let mut store = Store::memory().unwrap();
    store.log_gate(&report("o/r#1", at(1))).unwrap();
    store.log_gate(&report("o/r#2", at(10))).unwrap();
    store.log_gate(&report("o/r#3", at(20))).unwrap();

    // Strictly before: a row exactly at the cutoff is kept.
    assert_eq!(store.prune_gate_log(at(10)).unwrap(), 1);
    let left: Vec<String> = store
        .gate_reports(None)
        .unwrap()
        .iter()
        .map(|r| r.issue.to_string())
        .collect();
    assert_eq!(left, ["o/r#2", "o/r#3"]);

    assert_eq!(store.prune_gate_log(at(10)).unwrap(), 0);
    assert_eq!(
        store
            .prune_gate_log(at(20) + TimeDelta::seconds(1))
            .unwrap(),
        2
    );
    assert_eq!(store.stats().unwrap().rows["gate_log"], 0);
}

#[test]
fn stats_counts_gate_log_rows_and_bytes() {
    let mut store = Store::memory().unwrap();
    let empty = store.stats().unwrap();
    assert_eq!(empty.gate_log.rows, 0);
    assert_eq!(empty.gate_log.bytes, 0);

    let first = report("o/r#1", at(1));
    store.log_gate(&first).unwrap();
    store.log_gate(&report("o/r#22", at(2))).unwrap();
    let stats = store.stats().unwrap();
    assert_eq!(stats.gate_log.rows, 2);
    assert_eq!(stats.rows["gate_log"], 2);
    // At least the stored snapshots (each a whole gate report as JSON).
    let snapshot = serde_json::to_string(&first).unwrap().len() as u64;
    assert!(stats.gate_log.bytes >= 2 * snapshot, "{stats:?}");
}
