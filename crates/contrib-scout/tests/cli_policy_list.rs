//! `policy list` and `cache prune --gate-log-older-than` / `cache stats` gate-log counts.

use chrono::{TimeDelta, Utc};
use contrib_scout_core::models::GateReport;
use contrib_scout_core::store::Store;
use serde_json::{json, Value};
use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
    time::{SystemTime, UNIX_EPOCH},
};

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("{name}-{}-{unique}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        Self { root }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_contrib-scout"))
            .current_dir(&self.root)
            .env("CONTRIB_SCOUT_CONFIG", self.root.join("config.toml"))
            .env("CONTRIB_SCOUT_DB", self.root.join("scout.db"))
            .env_remove("CONTRIB_SCOUT_AGENT")
            .env("NO_COLOR", "1")
            .args(args)
            .output()
            .unwrap()
    }

    fn json(&self, args: &[&str]) -> Value {
        let mut all = vec!["--json"];
        all.extend_from_slice(args);
        let output = self.run(&all);
        assert!(output.status.success(), "{args:?}: {output:?}");
        let envelope: Value = serde_json::from_str(stdout(&output).trim()).unwrap();
        assert_eq!(envelope["schema"], "contrib-scout/v1", "{envelope}");
        envelope
    }

    fn human(&self, args: &[&str]) -> String {
        let output = self.run(args);
        assert!(output.status.success(), "{args:?}: {output:?}");
        stdout(&output)
    }

    fn set(&self, target: &str, stance: &str, note: &str) {
        self.json(&[
            "--offline",
            "--agent",
            "agent-a",
            "policy",
            "set",
            target,
            "--stance",
            stance,
            "--evidence",
            "https://example.org/ai-policy",
            "--note",
            note,
        ]);
    }

    fn db(&self) -> PathBuf {
        self.root.join("scout.db")
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn usage_envelope(output: &Output) -> Value {
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let envelope: Value = serde_json::from_str(stdout(output).trim()).unwrap();
    assert_eq!(envelope["schema"], "contrib-scout/v1", "{envelope}");
    assert_eq!(envelope["data"], Value::Null, "{envelope}");
    assert_eq!(envelope["error"]["code"], "usage", "{envelope}");
    assert_eq!(envelope["error"]["exit_code"], 2, "{envelope}");
    envelope
}

fn targets(envelope: &Value) -> Vec<String> {
    envelope["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|mark| mark["target"]["name"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn policy_list_with_no_marks() {
    let fixture = Fixture::new("policy-list-empty");
    let envelope = fixture.json(&["--offline", "policy", "list"]);
    assert_eq!(envelope["data"], json!([]));
    assert_eq!(envelope["meta"]["requests"], 0);
    let human = fixture.human(&["--offline", "policy", "list"]);
    assert!(human.contains("No policy marks stored."), "{human}");
    let human = fixture.human(&["policy", "list", "--stance", "bans", "--due"]);
    assert!(human.contains("No policy marks match."), "{human}");

    let help = fixture.human(&["policy", "list", "--help"]);
    assert!(
        help.contains("--stance") && help.contains("--due"),
        "{help}"
    );
    let help = fixture.human(&["cache", "prune", "--help"]);
    assert!(help.contains("--gate-log-older-than"), "{help}");
}

#[test]
fn policy_list_shows_repo_and_org_marks_sorted_by_target() {
    let fixture = Fixture::new("policy-list-all");
    fixture.set("zeta/repo", "welcomes", "Zeta welcomes AI");
    fixture.set("acme/widget", "bans", "No AI PRs");
    fixture.set("acme", "allows", "Org allows AI");
    fixture.set("Beta/thing", "none_found", "Nothing found");

    let envelope = fixture.json(&["--offline", "policy", "list"]);
    assert_eq!(envelope["meta"]["requests"], 0);
    assert_eq!(
        targets(&envelope),
        ["acme", "acme/widget", "Beta/thing", "zeta/repo"]
    );
    let marks = envelope["data"].as_array().unwrap();
    let org = &marks[0];
    assert_eq!(org["target"], json!({"kind": "org", "name": "acme"}));
    assert_eq!(org["kind"], "org");
    assert_eq!(org["stance"], "allows");
    assert_eq!(org["note"], "Org allows AI");
    assert_eq!(org["marked_by"], "agent-a");
    assert!(org["marked_at"].is_string(), "{org}");
    assert!(org["expires_at"].is_string(), "{org}");
    assert_eq!(org["review_due"], false);
    // The full mark is there too, as in `policy due`.
    assert_eq!(org["evidence"][0]["url"], "https://example.org/ai-policy");
    let repo = &marks[1];
    assert_eq!(repo["kind"], "repo");
    assert_eq!(repo["stance"], "bans");

    let human = fixture.human(&["--offline", "policy", "list"]);
    let lines: Vec<&str> = human.lines().collect();
    assert!(
        lines[0].starts_with("org acme: Allows AI — \"Org allows AI\""),
        "{human}"
    );
    assert!(lines[1].starts_with("repo acme/widget: Bans AI"), "{human}");
    assert!(human.contains("4 mark(s)."), "{human}");

    // NDJSON: one envelope per mark.
    let output = fixture.run(&["--ndjson", "policy", "list"]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(stdout(&output).lines().count(), 4);
}

#[test]
fn policy_list_filters_by_stance() {
    let fixture = Fixture::new("policy-list-stance");
    fixture.set("a/one", "bans", "one");
    fixture.set("b/two", "allows", "two");
    fixture.set("c", "bans", "three");
    fixture.set("d/four", "welcomes", "four");

    let bans = fixture.json(&["policy", "list", "--stance", "bans"]);
    assert_eq!(targets(&bans), ["a/one", "c"]);
    // The same lenient-case, `-` for `_` parser `policy set` uses.
    let repeated = fixture.json(&[
        "policy", "list", "--stance", "Allows", "--stance", "welcomes",
    ]);
    assert_eq!(targets(&repeated), ["b/two", "d/four"]);
    let none = fixture.json(&["policy", "list", "--stance", "none-found"]);
    assert_eq!(none["data"], json!([]));
}

#[test]
fn policy_list_rejects_an_unknown_stance() {
    let fixture = Fixture::new("policy-list-bad-stance");
    let output = fixture.run(&["--json", "policy", "list", "--stance", "maybe"]);
    usage_envelope(&output);
    let output = fixture.run(&["policy", "list", "--stance", "maybe"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("welcomes"), "{stderr}");
}

#[test]
fn policy_list_due_matches_policy_due() {
    let fixture = Fixture::new("policy-list-due");
    let output = fixture.run(&["config", "set", "mark_ttl_bans", "0s"]);
    assert!(output.status.success(), "{output:?}");
    fixture.set("old/repo", "bans", "Expired \u{1b}[31mred\u{1b}[0m note");
    fixture.set("new/repo", "allows", "Still fresh");
    fixture.set("acme", "bans", "Org expired");

    let all = fixture.json(&["--offline", "policy", "list"]);
    let flags: Vec<(String, bool)> = all["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            (
                m["target"]["name"].as_str().unwrap().to_owned(),
                m["review_due"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        flags,
        [
            ("acme".to_owned(), true),
            ("new/repo".to_owned(), false),
            ("old/repo".to_owned(), true),
        ]
    );

    let due = fixture.json(&["--offline", "policy", "list", "--due"]);
    assert_eq!(targets(&due), ["acme", "old/repo"]);
    assert_eq!(due["meta"]["requests"], 0);
    let combined = fixture.json(&["policy", "list", "--due", "--stance", "allows"]);
    assert_eq!(combined["data"], json!([]));

    // `policy due` still lists the same marks, with the same per-mark object.
    let classic = fixture.json(&["--offline", "policy", "due"]);
    let mut classic_targets = targets(&classic);
    classic_targets.sort();
    assert_eq!(classic_targets, ["acme", "old/repo"]);
    for mark in classic["data"].as_array().unwrap() {
        assert_eq!(mark["review_due"], true, "{mark}");
        let name = mark["target"]["name"].as_str().unwrap();
        let listed = due["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["target"]["name"] == name)
            .unwrap();
        assert_eq!(mark, listed);
    }
    assert_eq!(classic["meta"]["reminders"][0]["repo"], "old/repo");
    assert_eq!(classic["meta"]["reminders"][0]["review_due"], true);

    // Human lines are sanitised like every other human line.
    let human = fixture.human(&["policy", "list", "--due"]);
    assert!(!human.contains('\u{1b}'), "{human:?}");
    assert!(human.contains("red"), "{human}");
    assert!(human.contains("(now)"), "{human}");
    assert!(human.contains("2 mark(s)."), "{human}");
}

fn gate_report(issue: &str, age: TimeDelta) -> GateReport {
    serde_json::from_value(json!({
        "kind": "pr",
        "issue": issue,
        "agent": "agent-a",
        "verdict": "blocked",
        "raw": {},
        "as_of": Utc::now() - age,
    }))
    .unwrap()
}

#[test]
fn cache_prune_gate_log_only_with_the_flag() {
    let fixture = Fixture::new("prune-gate-log");
    // Create the database through the CLI, then log gates as `gate` would.
    fixture.json(&["cache", "stats"]);
    {
        let mut store = Store::open(fixture.db()).unwrap();
        store
            .log_gate(&gate_report("o/r#1", TimeDelta::days(100)))
            .unwrap();
        store
            .log_gate(&gate_report("o/r#2", TimeDelta::days(40)))
            .unwrap();
        store
            .log_gate(&gate_report("o/r#3", TimeDelta::days(1)))
            .unwrap();
    }

    let stats = fixture.json(&["cache", "stats"]);
    assert_eq!(stats["data"]["rows"]["gate_log"], 3);
    assert_eq!(stats["data"]["gate_log"]["rows"], 3);
    assert!(
        stats["data"]["gate_log"]["bytes"].as_u64().unwrap() > 0,
        "{stats}"
    );
    let human = fixture.human(&["cache", "stats"]);
    assert!(human.contains("Gate log"), "{human}");

    // Without the flag the gate log is untouched, as before.
    let pruned = fixture.json(&["cache", "prune", "--older-than", "1d"]);
    assert!(
        pruned["data"]["deleted"].get("gate_log").is_none(),
        "{pruned}"
    );
    assert_eq!(pruned["data"]["gate_log_cutoff"], Value::Null);
    let stats = fixture.json(&["cache", "stats"]);
    assert_eq!(stats["data"]["gate_log"]["rows"], 3);

    let pruned = fixture.json(&["cache", "prune", "--gate-log-older-than", "30d"]);
    assert_eq!(pruned["data"]["deleted"]["gate_log"], 2, "{pruned}");
    assert!(pruned["data"]["gate_log_cutoff"].is_string(), "{pruned}");
    assert_eq!(pruned["data"]["total"].as_u64().unwrap(), 2, "{pruned}");
    let stats = fixture.json(&["cache", "stats"]);
    assert_eq!(stats["data"]["gate_log"]["rows"], 1);
    let left = Store::open(fixture.db())
        .unwrap()
        .gate_reports(None)
        .unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].issue.to_string(), "o/r#3");

    let human = fixture.human(&["cache", "prune", "--gate-log-older-than", "0s"]);
    assert!(human.contains("gate_log"), "{human}");
    assert_eq!(
        fixture.json(&["cache", "stats"])["data"]["gate_log"]["rows"],
        0
    );
}

#[test]
fn cache_prune_gate_log_age_overflow_is_a_usage_error() {
    let fixture = Fixture::new("prune-gate-log-overflow");
    for age in ["100000000d", "99999999999999999d", "soon"] {
        let output = fixture.run(&["--json", "cache", "prune", "--gate-log-older-than", age]);
        assert_ne!(output.status.code(), Some(101), "{age}: {output:?}");
        usage_envelope(&output);
    }
}
