//! `candidates`: claim awareness and `--repo` / `--label` filters, end to end.

use chrono::{TimeDelta, Utc};
use contrib_scout_core::models::{IssueRow, IssueState};
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

    fn store(&self) -> Store {
        Store::open(self.root.join("scout.db")).unwrap()
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn envelope(output: &Output) -> Value {
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let envelope: Value = serde_json::from_str(stdout(output).trim()).unwrap();
    assert_eq!(envelope["schema"], "contrib-scout/v1", "{envelope}");
    assert_eq!(envelope["meta"]["requests"], 0, "zero requests: {envelope}");
    envelope
}

fn listed(envelope: &Value) -> Vec<String> {
    let mut issues: Vec<String> = envelope["data"]["candidates"]
        .as_array()
        .unwrap_or_else(|| panic!("{envelope}"))
        .iter()
        .map(|c| c["issue"]["issue"].as_str().unwrap().to_owned())
        .collect();
    issues.sort();
    issues
}

fn row(issue: &str, labels: &[&str]) -> (IssueRow, Value) {
    let now = Utc::now();
    (
        IssueRow {
            issue: issue.parse().unwrap(),
            title: format!("title of {issue}"),
            state: IssueState::Open,
            labels: labels.iter().map(|l| (*l).to_owned()).collect(),
            assignees: Vec::new(),
            comments: 0,
            created_at: Some(now - TimeDelta::days(5)),
            updated_at: now - TimeDelta::days(1),
            fetched_at: now,
        },
        json!({}),
    )
}

/// a/one#1 (claimed by agent-b), a/one#2 (claimed by agent-a), a/one#3
/// (expired claim), b/two#4 (unclaimed, labels differ).
fn seeded(name: &str) -> Fixture {
    let fixture = Fixture::new(name);
    let mut store = fixture.store();
    store
        .save_issues(&[
            row("a/one#1", &["good first issue"]),
            row("a/one#2", &["good first issue", "help wanted"]),
            row("a/one#3", &["Good First Issue"]),
            row("b/two#4", &["Help Wanted"]),
        ])
        .unwrap();
    let now = Utc::now();
    let claim = |store: &mut Store, issue: &str, agent: &str, start: chrono::DateTime<Utc>| {
        store
            .claim_issue(
                &issue.parse().unwrap(),
                &agent.parse().unwrap(),
                start,
                start + TimeDelta::hours(1),
                None,
            )
            .unwrap();
    };
    claim(&mut store, "a/one#1", "agent-b", now);
    claim(&mut store, "a/one#2", "agent-a", now);
    claim(&mut store, "a/one#3", "agent-b", now - TimeDelta::hours(3));
    fixture
}

#[test]
fn candidates_hide_claims_held_by_others_and_show_own() {
    let fixture = seeded("cand-claims");

    let mine = envelope(&fixture.run(&["--json", "--agent", "agent-a", "candidates"]));
    assert_eq!(listed(&mine), ["a/one#2", "a/one#3", "b/two#4"]);
    assert_eq!(mine["data"]["excluded_claimed"], 1);
    let own = mine["data"]["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["issue"]["issue"] == "a/one#2")
        .unwrap();
    assert_eq!(own["claim"]["agent"], "agent-a");
    assert!(own["claim"]["lease_until"].is_string(), "{own}");

    // The agent id may also come from the environment.
    let output = Command::new(env!("CARGO_BIN_EXE_contrib-scout"))
        .current_dir(&fixture.root)
        .env("CONTRIB_SCOUT_CONFIG", fixture.root.join("config.toml"))
        .env("CONTRIB_SCOUT_DB", fixture.root.join("scout.db"))
        .env("CONTRIB_SCOUT_AGENT", "agent-b")
        .env("NO_COLOR", "1")
        .args(["--json", "candidates"])
        .output()
        .unwrap();
    let theirs = envelope(&output);
    assert_eq!(listed(&theirs), ["a/one#1", "a/one#3", "b/two#4"]);
    assert_eq!(theirs["data"]["excluded_claimed"], 1);

    // Without an agent id every active claim is hidden; candidates still works.
    let anonymous = envelope(&fixture.run(&["--json", "candidates"]));
    assert_eq!(listed(&anonymous), ["a/one#3", "b/two#4"]);
    assert_eq!(anonymous["data"]["excluded_claimed"], 2);
    assert_eq!(anonymous["data"]["excluded_marked"], 0);
    assert_eq!(anonymous["data"]["excluded_skipped"], 0);

    let human = stdout(&fixture.run(&["candidates"]));
    assert!(
        human.contains(
            "Excluded 2 issues claimed by other agents (use --include-claimed to show them)"
        ),
        "{human}"
    );
    let human = stdout(&fixture.run(&["--agent", "agent-a", "candidates"]));
    let line = human
        .lines()
        .find(|l| l.contains("a/one#2"))
        .unwrap_or_else(|| panic!("{human}"));
    assert!(line.contains("(claimed by agent-a until "), "{line}");

    // --include-claimed lists them all, each with its claim.
    let all = envelope(&fixture.run(&["--json", "candidates", "--include-claimed"]));
    assert_eq!(listed(&all), ["a/one#1", "a/one#2", "a/one#3", "b/two#4"]);
    assert_eq!(all["data"]["excluded_claimed"], 0);
    for candidate in all["data"]["candidates"].as_array().unwrap() {
        let expected = match candidate["issue"]["issue"].as_str().unwrap() {
            "a/one#1" => json!("agent-b"),
            "a/one#2" => json!("agent-a"),
            _ => Value::Null,
        };
        assert_eq!(candidate["claim"]["agent"], expected, "{candidate}");
        if expected.is_null() {
            assert!(candidate.get("claim").is_none(), "{candidate}");
        }
    }
    let human = stdout(&fixture.run(&["candidates", "--include-claimed"]));
    assert!(human.contains("(claimed by agent-b until "), "{human}");
    assert!(!human.contains("Excluded"), "{human}");

    // Releasing the claim makes the issue visible to everyone again.
    let released = fixture
        .store()
        .release_claim(
            &"a/one#1".parse().unwrap(),
            &"agent-b".parse().unwrap(),
            Utc::now(),
        )
        .unwrap();
    assert!(released);
    let after = envelope(&fixture.run(&["--json", "--agent", "agent-a", "candidates"]));
    assert_eq!(listed(&after), ["a/one#1", "a/one#2", "a/one#3", "b/two#4"]);
    assert_eq!(after["data"]["excluded_claimed"], 0);
}

#[test]
fn candidates_filter_by_repo_and_label() {
    let fixture = seeded("cand-filters");
    let run = |extra: &[&str]| {
        let mut args = vec!["--json", "candidates", "--include-claimed"];
        args.extend_from_slice(extra);
        envelope(&fixture.run(&args))
    };

    assert_eq!(listed(&run(&["--repo", "B/Two"])), ["b/two#4"]);
    assert_eq!(
        listed(&run(&[
            "--repo",
            "a/one",
            "--repo",
            "https://github.com/b/two"
        ])),
        ["a/one#1", "a/one#2", "a/one#3", "b/two#4"]
    );
    assert_eq!(
        listed(&run(&["--label", "GOOD FIRST ISSUE"])),
        ["a/one#1", "a/one#2", "a/one#3"]
    );
    assert_eq!(
        listed(&run(&[
            "--label",
            "good first issue",
            "--label",
            "help WANTED"
        ])),
        ["a/one#2"]
    );
    assert_eq!(
        listed(&run(&["--repo", "b/two", "--label", "help wanted"])),
        ["b/two#4"]
    );
    assert_eq!(listed(&run(&["--label", "missing"])), Vec::<String>::new());

    // Claimed issues outside the filters are not counted as excluded.
    let filtered = envelope(&fixture.run(&["--json", "candidates", "--repo", "b/two"]));
    assert_eq!(filtered["data"]["excluded_claimed"], 0);

    // --repo uses the same owner/repo parser as other commands.
    for bad in ["not-a-repo", "a/b/c"] {
        let output = fixture.run(&["--json", "candidates", "--repo", bad]);
        assert_eq!(output.status.code(), Some(2), "{bad}: {output:?}");
        let envelope: Value = serde_json::from_str(stdout(&output).trim()).unwrap();
        assert_eq!(envelope["error"]["code"], "usage", "{envelope}");
    }
    let output = fixture.run(&["--json", "candidates", "--label", " "]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");

    let help = stdout(&fixture.run(&["candidates", "--help"]));
    for flag in ["--include-claimed", "--repo", "--label"] {
        assert!(help.contains(flag), "{flag}: {help}");
    }
    assert!(
        help.contains("all"),
        "label semantics are documented: {help}"
    );
}

#[test]
fn candidates_work_offline_with_zero_requests() {
    let fixture = seeded("cand-offline");
    let output = fixture.run(&[
        "--json",
        "--offline",
        "--agent",
        "agent-a",
        "candidates",
        "--repo",
        "a/one",
        "--label",
        "good first issue",
    ]);
    let envelope = envelope(&output);
    assert_eq!(listed(&envelope), ["a/one#2", "a/one#3"]);
    assert_eq!(envelope["data"]["excluded_claimed"], 1);
    assert_eq!(envelope["meta"]["requests"], 0);
}
