use serde_json::Value;
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
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

const COMMANDS: &[&[&str]] = &[
    &[],
    &["discover"],
    &["repo"],
    &["repo", "show"],
    &["policy"],
    &["policy", "files"],
    &["policy", "show"],
    &["policy", "set"],
    &["policy", "unset"],
    &["policy", "due"],
    &["candidates"],
    &["issue"],
    &["issue", "show"],
    &["issue", "mark"],
    &["issue", "unmark"],
    &["issue", "marks"],
    &["gate"],
    &["gate", "start"],
    &["gate", "pr"],
    &["claim"],
    &["claim", "list"],
    &["claim", "renew"],
    &["claim", "release"],
    &["track"],
    &["track", "add"],
    &["track", "rm"],
    &["track", "list"],
    &["sync"],
    &["quota"],
    &["cache"],
    &["cache", "stats"],
    &["cache", "prune"],
    &["config"],
    &["config", "get"],
    &["config", "set"],
];

#[test]
fn every_command_has_help() {
    let fixture = Fixture::new("help");
    for command in COMMANDS {
        let mut args = command.to_vec();
        args.push("--help");
        let output = fixture.run(&args);
        assert!(output.status.success(), "{args:?} failed: {output:?}");
        let text = stdout(&output);
        assert!(text.contains("Usage:"), "{args:?}: {text}");
        assert!(
            text.contains("--json"),
            "{args:?} lacks global options: {text}"
        );
    }
}

#[test]
fn stubs_return_a_json_error_envelope() {
    let fixture = Fixture::new("stub");
    let stubs: &[&[&str]] = &[
        &["--json", "gate", "start", "https://github.com/a/b/issues/1"],
        &["--json", "gate", "pr", "a/b#1", "--branch", "x"],
        &["--json", "claim", "list"],
    ];
    for args in stubs {
        let output = fixture.run(args);
        assert_eq!(output.status.code(), Some(1), "{args:?}: {output:?}");
        let envelope: Value = serde_json::from_str(stdout(&output).trim()).unwrap();
        assert_eq!(envelope["schema"], "contrib-scout/v1", "{args:?}");
        assert_eq!(envelope["error"]["code"], "not_implemented", "{args:?}");
        assert!(envelope["meta"]["as_of"].is_string());
    }
    let human = fixture.run(&["claim", "list"]);
    assert_eq!(human.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&human.stderr).contains("not implemented"));
}

#[test]
fn usage_errors_exit_2() {
    let fixture = Fixture::new("usage");
    for args in [
        &["repo", "show", "not-a-repo"][..],
        &["issue", "show", "a/b"],
        &["--json", "--ndjson", "quota"],
        &["--offline", "--fresh", "quota"],
        &[
            "policy",
            "set",
            "a/b",
            "--stance",
            "maybe",
            "--evidence",
            "x",
            "--note",
            "n",
        ],
        &["policy", "set", "a/b", "--stance", "bans", "--note", "n"],
        &[
            "policy",
            "set",
            "a/b",
            "--stance",
            "bans",
            "--evidence",
            "x",
            "--note",
            "n",
            "--scope",
            "bogus=1",
        ],
        &["--max-requests", "0", "quota"],
        &["sync"],
        &["sync", "--all-tracked", "a/b"],
        &["issue", "mark", "a/b#1", "--verdict", "skip", "--note", "n"],
        &[
            "--agent",
            "a1",
            "issue",
            "mark",
            "a/b#1",
            "--verdict",
            "skip",
            "--note",
            "n",
        ],
        &[
            "--agent",
            "a1",
            "issue",
            "mark",
            "a/b#1",
            "--verdict",
            "shortlist",
            "--reason",
            "other",
            "--note",
            "n",
        ],
        &[
            "--agent",
            "a1",
            "issue",
            "mark",
            "a/b#1",
            "--verdict",
            "skip",
            "--reason",
            "lazy",
            "--note",
            "n",
        ],
        &[
            "--agent",
            "a1",
            "issue",
            "mark",
            "a/b#1",
            "--verdict",
            "skip",
            "--reason",
            "other",
            "--note",
            "n",
            "--until",
            "soon",
        ],
        &["issue", "unmark", "a/b#1"],
        &["cache", "prune", "--older-than", "soon"],
        &["frobnicate"],
    ] {
        let output = fixture.run(args);
        assert_eq!(output.status.code(), Some(2), "{args:?}: {output:?}");
    }
}

#[test]
fn config_get_and_set() {
    let fixture = Fixture::new("config");
    let output = fixture.run(&["--json", "config", "get", "claim_lease"]);
    assert!(output.status.success(), "{output:?}");
    let envelope: Value = serde_json::from_str(stdout(&output).trim()).unwrap();
    assert_eq!(
        envelope["data"],
        serde_json::json!({"key": "claim_lease", "value": "48h"})
    );

    let output = fixture.run(&["config", "set", "claim_lease", "24h"]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(stdout(&output).trim(), "claim_lease = \"24h\"");
    let output = fixture.run(&["config", "get", "claim_lease"]);
    assert_eq!(stdout(&output).trim(), "claim_lease = \"24h\"");

    let output = fixture.run(&["--json", "config", "set", "claim_lease", "soon"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let envelope: Value = serde_json::from_str(stdout(&output).trim()).unwrap();
    assert_eq!(envelope["error"]["code"], "usage");
    let output = fixture.run(&["config", "get", "nope"]);
    assert_eq!(output.status.code(), Some(2));
}

// --- step 4: discover, repo show, candidates ---

fn envelope_of(output: &Output) -> Value {
    serde_json::from_str(stdout(output).trim()).unwrap()
}

#[test]
fn candidates_on_an_empty_index_makes_zero_requests() {
    let fixture = Fixture::new("candidates");
    let output = fixture.run(&["--json", "candidates", "--min-score", "10"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let envelope = envelope_of(&output);
    assert_eq!(envelope["data"]["candidates"], serde_json::json!([]));
    assert_eq!(envelope["data"]["excluded_marked"], 0);
    assert_eq!(envelope["meta"]["requests"], 0);
    let human = fixture.run(&["candidates"]);
    assert!(stdout(&human).contains("No candidates"), "{human:?}");
}

#[test]
fn discover_explain_makes_no_requests() {
    let fixture = Fixture::new("explain");
    let output = fixture.run(&[
        "--json",
        "discover",
        "--explain",
        "--lang",
        "rust",
        "--limit",
        "5",
    ]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let envelope = envelope_of(&output);
    assert_eq!(envelope["meta"]["requests"], 0);
    assert_eq!(envelope["data"]["estimated_requests"], 1 + 3 * 5);
    assert!(envelope["data"]["query"]
        .as_str()
        .unwrap()
        .contains("language:rust"));
    let human = fixture.run(&["discover", "--explain"]);
    assert!(stdout(&human).contains("Estimated requests"), "{human:?}");
}

#[test]
fn offline_discover_without_cache_fails_without_requests() {
    let fixture = Fixture::new("discover-offline");
    let output = fixture.run(&["--json", "--offline", "discover"]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert_eq!(envelope_of(&output)["error"]["code"], "not_found");
}

#[test]
fn repo_show_includes_the_policy_reminder() {
    use contrib_scout_core::models::{Evidence, PolicyMark, PolicyScope, RepoProfile, Stance};
    use contrib_scout_core::store::Store;
    let fixture = Fixture::new("repo-show");
    let output = fixture.run(&["--json", "--offline", "repo", "show", "a/b"]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert_eq!(envelope_of(&output)["error"]["code"], "not_found");

    let now = chrono::Utc::now();
    let db = fixture.root.join("scout.db");
    let profile = RepoProfile {
        repo: "a/b".parse().unwrap(),
        description: Some("demo".into()),
        archived: false,
        fork: false,
        license: Some("MIT".into()),
        default_branch: Some("main".into()),
        language: Some("Rust".into()),
        topics: vec!["cli".into()],
        stars: 7,
        pushed_at: Some(now),
        fetched_at: now,
        activity: None,
    };
    Store::open(&db)
        .unwrap()
        .save_repo(&profile, &serde_json::json!({}), Some("\"e\""))
        .unwrap();
    let output = fixture.run(&["--json", "--offline", "repo", "show", "a/b"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let json = envelope_of(&output);
    assert_eq!(json["data"]["profile"]["stars"], 7);
    assert_eq!(json["meta"]["requests"], 0);
    assert_eq!(json["meta"]["reminders"][0]["kind"], "unmarked");
    let human = fixture.run(&["--offline", "repo", "show", "a/b"]);
    assert!(
        stdout(&human).contains("a/b: AI policy not marked"),
        "{human:?}"
    );

    Store::open(&db)
        .unwrap()
        .put_mark(&PolicyMark {
            target: "a".parse().unwrap(),
            stance: Stance::Bans,
            scope: PolicyScope::default(),
            requirements: Vec::new(),
            evidence: vec![Evidence {
                path: "CONTRIBUTING.md".into(),
                lines: None,
                url: "https://github.com/a/.github/blob/x/CONTRIBUTING.md".into(),
                excerpt: "No AI.".into(),
            }],
            note: "org bans AI".into(),
            marked_by: "agent-a".parse().unwrap(),
            marked_at: now,
            expires_at: now + chrono::TimeDelta::days(90),
        })
        .unwrap();
    let json = envelope_of(&fixture.run(&["--json", "--offline", "repo", "show", "a/b"]));
    assert_eq!(json["meta"]["reminders"][0]["kind"], "marked");
    assert_eq!(json["meta"]["reminders"][0]["note"], "org bans AI");
    let text = stdout(&fixture.run(&["--offline", "repo", "show", "a/b"]));
    assert!(text.contains("org bans AI"), "{text}");
    assert!(
        text.contains("activity not computed (repository marked Bans)"),
        "{text}"
    );
}

fn json(output: &Output) -> Value {
    serde_json::from_str(stdout(output).trim()).unwrap_or_else(|e| panic!("{e}: {output:?}"))
}

#[test]
fn track_and_issue_marks_round_trip() {
    let fixture = Fixture::new("issue-marks");

    let output = fixture.run(&["--json", "--agent", "a1", "track", "add", "a/b"]);
    assert!(output.status.success(), "{output:?}");
    let envelope = json(&output);
    assert_eq!(envelope["data"]["added"], true);
    assert_eq!(envelope["meta"]["reminders"][0]["kind"], "unmarked");
    let output = fixture.run(&["--json", "track", "list"]);
    assert_eq!(json(&output)["data"][0]["repo"], "a/b");
    let output = fixture.run(&["track", "list"]);
    let text = stdout(&output);
    assert!(text.contains("AI policy not marked"), "{text}");
    assert!(text.contains("a/b  (added "), "{text}");

    let output = fixture.run(&[
        "--json",
        "--agent",
        "a1",
        "issue",
        "mark",
        "a/b#1",
        "--verdict",
        "skip",
        "--reason",
        "too-large",
        "--note",
        "Needs a rewrite",
        "--until",
        "30d",
    ]);
    assert!(output.status.success(), "{output:?}");
    let envelope = json(&output);
    assert_eq!(envelope["data"]["mark"]["verdict"], "skip");
    assert_eq!(envelope["data"]["mark"]["reason"], "too_large");
    assert_eq!(envelope["data"]["mark"]["seen_updated_at"], Value::Null);
    assert!(envelope["data"]["mark"]["expires_at"].is_string());
    let kinds: Vec<_> = envelope["meta"]["reminders"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["kind"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(kinds, ["unmarked", "issue_marked"]);

    let output = fixture.run(&["--json", "issue", "marks"]);
    let envelope = json(&output);
    assert_eq!(envelope["data"].as_array().unwrap().len(), 1);
    assert_eq!(envelope["data"][0]["mark"]["issue"], "a/b#1");
    assert_eq!(envelope["data"][0]["active"], true);
    let output = fixture.run(&["--json", "issue", "marks", "--verdict", "shortlist"]);
    assert_eq!(json(&output)["data"], serde_json::json!([]));
    let output = fixture.run(&["issue", "marks", "--repo", "a/b"]);
    let text = stdout(&output);
    assert!(text.contains("a/b#1 skip (too_large)"), "{text}");

    let output = fixture.run(&["--agent", "a1", "issue", "unmark", "a/b#1"]);
    assert!(output.status.success(), "{output:?}");
    assert!(stdout(&output).contains("Removed the skip mark on a/b#1"));
    let output = fixture.run(&["--json", "--agent", "a1", "issue", "unmark", "a/b#1"]);
    assert_eq!(json(&output)["data"]["removed"], false);
    let output = fixture.run(&["--json", "issue", "marks"]);
    assert_eq!(json(&output)["data"], serde_json::json!([]));

    let output = fixture.run(&["--json", "track", "rm", "a/b"]);
    assert_eq!(json(&output)["data"]["removed"], true);
}

#[test]
fn sync_and_issue_show_offline() {
    let fixture = Fixture::new("sync-offline");
    let output = fixture.run(&["--json", "--offline", "sync", "a/b", "A/B"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let envelope = json(&output);
    let repos = envelope["data"]["repos"].as_array().unwrap();
    assert_eq!(repos.len(), 1, "duplicates are synced once");
    assert_eq!(repos[0]["skipped"], "offline");
    assert_eq!(envelope["meta"]["requests"], 0);
    assert_eq!(envelope["meta"]["reminders"][0]["repo"], "a/b");

    let output = fixture.run(&["--json", "--offline", "sync", "--all-tracked"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(json(&output)["data"]["repos"], serde_json::json!([]));

    let output = fixture.run(&["--json", "--offline", "issue", "show", "a/b#1"]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert_eq!(json(&output)["error"]["code"], "not_found");
}

// ---------------------------------------------------------------------------
// policy / quota / cache (step 3)
// ---------------------------------------------------------------------------

const SEED_SHA: &str = "2222222222222222222222222222222222222222";
const SEED_CONTRIB: &str =
    "# Contributing\nWe do not accept AI-generated pull requests.\nThis includes LLM-written code.\nThanks!\n";

fn json_out(output: &Output) -> Value {
    serde_json::from_str(stdout(output).trim())
        .unwrap_or_else(|e| panic!("bad JSON ({e}): {output:?}"))
}

/// Seeds the policy-file cache for `acme/widget` (and a missing `acme/.github`)
/// as a previous `policy files` run would have, so `--offline` works.
fn seed_policy_files(fixture: &Fixture) {
    use contrib_scout_core::models::PolicyFile;
    use contrib_scout_core::store::{CachedPolicyFile, Store};
    use contrib_scout_core::RepoName;
    let mut store = Store::open(fixture.root.join("scout.db")).unwrap();
    let now = chrono::Utc::now();
    let repo: RepoName = "acme/widget".parse().unwrap();
    let org: RepoName = "acme/.github".parse().unwrap();
    let url = |path: &str| format!("https://github.com/acme/widget/blob/{SEED_SHA}/{path}");
    let row = |repo: &RepoName, path: &str, status: u16| CachedPolicyFile {
        repo: repo.clone(),
        path: path.into(),
        status,
        file: None,
        commit: Some(SEED_SHA.into()),
        url: (status == 404 && !path.is_empty()).then(|| url(path)),
        etag: None,
        fetched_at: now,
    };
    let mut contrib = row(&repo, "CONTRIBUTING.md", 200);
    contrib.commit = None;
    contrib.url = Some(url("CONTRIBUTING.md"));
    contrib.file = Some(PolicyFile {
        repo: repo.clone(),
        path: "CONTRIBUTING.md".into(),
        url: url("CONTRIBUTING.md"),
        text: Some(SEED_CONTRIB.into()),
        truncated: false,
        size: SEED_CONTRIB.len() as u64,
        sha: Some("blob1".into()),
        fetched_at: now,
    });
    store
        .replace_policy_files(
            &repo,
            &[
                row(&repo, "", 200),
                contrib,
                row(&repo, "AI_POLICY.md", 404),
            ],
        )
        .unwrap();
    let mut missing = row(&org, "", 404);
    missing.commit = None;
    store.replace_policy_files(&org, &[missing]).unwrap();
}

#[test]
fn policy_files_offline_serves_the_cache() {
    let fixture = Fixture::new("policy-files");
    seed_policy_files(&fixture);
    let output = fixture.run(&["--offline", "--json", "policy", "files", "acme/widget"]);
    assert!(output.status.success(), "{output:?}");
    let envelope = json_out(&output);
    assert_eq!(envelope["meta"]["requests"], 0);
    let files = envelope["data"]["files"].as_array().unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0]["text"], SEED_CONTRIB);
    assert_eq!(envelope["meta"]["reminders"][0]["kind"], "unmarked");

    let human = fixture.run(&["--offline", "policy", "files", "acme/widget"]);
    let text = stdout(&human);
    assert!(text.contains("AI policy not marked"), "{text}");
    assert!(text.contains("We do not accept AI-generated"), "{text}");
    assert!(
        text.contains("Absent in acme/widget: AI_POLICY.md"),
        "{text}"
    );

    // Nothing cached and offline: an error, no network.
    let output = fixture.run(&["--offline", "--json", "policy", "files", "other/repo"]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert_eq!(json_out(&output)["error"]["code"], "not_found");
}

#[test]
fn policy_set_show_due_unset_round_trip() {
    let fixture = Fixture::new("policy-round-trip");
    seed_policy_files(&fixture);
    let set = |extra: &[&str]| {
        let mut args = vec![
            "--offline",
            "--json",
            "--agent",
            "agent-a",
            "policy",
            "set",
            "acme/widget",
            "--stance",
            "bans",
            "--note",
            "No AI PRs",
        ];
        args.extend_from_slice(extra);
        fixture.run(&args)
    };

    // Agent id is required.
    let output = fixture.run(&[
        "--offline",
        "policy",
        "set",
        "acme/widget",
        "--stance",
        "bans",
        "--evidence",
        "CONTRIBUTING.md",
        "--note",
        "n",
    ]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    // More than three lines quoted.
    let output = set(&["--evidence", "CONTRIBUTING.md:1-4"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    // Not cached (and offline): ask for `policy files` first.
    let output = set(&["--evidence", "docs/POLICY.md"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(json_out(&output)["error"]["message"]
        .as_str()
        .unwrap()
        .contains("policy files"));
    // Lines on an absent path.
    let output = set(&["--evidence", "AI_POLICY.md:1"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");

    let output = set(&[
        "--evidence",
        "contributing.md:2-3",
        "--evidence",
        "AI_POLICY.md",
        "--evidence",
        "https://example.org/ai-policy",
        "--forbid-label",
        "good first issue",
        "--require",
        "assisted_by_trailer",
        "--scope",
        "pr_text=human_written",
    ]);
    assert!(output.status.success(), "{output:?}");
    let envelope = json_out(&output);
    assert_eq!(envelope["meta"]["requests"], 0);
    let mark = &envelope["data"]["mark"];
    assert_eq!(mark["stance"], "bans");
    assert_eq!(mark["marked_by"], "agent-a");
    let evidence = mark["evidence"].as_array().unwrap();
    assert_eq!(evidence[0]["path"], "CONTRIBUTING.md");
    assert_eq!(
        evidence[0]["url"],
        format!("https://github.com/acme/widget/blob/{SEED_SHA}/CONTRIBUTING.md")
    );
    assert_eq!(
        evidence[0]["excerpt"],
        "We do not accept AI-generated pull requests.\nThis includes LLM-written code."
    );
    assert_eq!(evidence[1]["path"], "AI_POLICY.md");
    assert_eq!(evidence[1]["excerpt"], "");
    assert_eq!(evidence[2]["url"], "https://example.org/ai-policy");
    assert_eq!(mark["scope"]["forbidden_labels"][0], "good first issue");
    assert_eq!(mark["scope"]["pr_text"], "human_written");
    assert_eq!(mark["requirements"][0], "assisted_by_trailer");
    let reminder = &envelope["meta"]["reminders"][0];
    assert_eq!(reminder["kind"], "marked");
    assert_eq!(reminder["note"], "No AI PRs");

    // show: the mark, the effective mark, and history.
    let output = fixture.run(&["--json", "policy", "show", "acme/widget"]);
    assert!(output.status.success(), "{output:?}");
    let shown = json_out(&output);
    assert_eq!(shown["data"]["mark"]["note"], "No AI PRs");
    assert_eq!(shown["data"]["effective"]["note"], "No AI PRs");
    assert_eq!(shown["data"]["history"].as_array().unwrap().len(), 1);
    let human = stdout(&fixture.run(&["policy", "show", "acme/widget"]));
    assert!(
        human.contains("acme/widget: Bans AI — \"No AI PRs\""),
        "{human}"
    );
    assert!(human.contains("> We do not accept"), "{human}");

    // An org mark with URL evidence, expiring immediately, shows up as due.
    let output = fixture.run(&["config", "set", "mark_ttl_allows", "0s"]);
    assert!(output.status.success(), "{output:?}");
    let output = fixture.run(&[
        "--json",
        "--agent",
        "agent-b",
        "policy",
        "set",
        "acme",
        "--stance",
        "allows",
        "--evidence",
        "https://acme.example/ai",
        "--note",
        "Org allows AI",
    ]);
    assert!(output.status.success(), "{output:?}");
    let output = fixture.run(&["--json", "policy", "due"]);
    assert!(output.status.success(), "{output:?}");
    let due = json_out(&output);
    let due = due["data"].as_array().unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(
        due[0]["target"],
        serde_json::json!({"kind": "org", "name": "acme"})
    );

    // Repos under the org but without their own mark see the org mark.
    let shown = json_out(&fixture.run(&["--json", "policy", "show", "acme/gadget"]));
    assert!(shown["data"]["mark"].is_null());
    assert_eq!(shown["data"]["effective"]["note"], "Org allows AI");
    assert_eq!(shown["meta"]["reminders"][0]["review_due"], true);

    // unset: the repo falls back to the org mark; history is kept.
    let output = fixture.run(&[
        "--json",
        "--agent",
        "agent-a",
        "policy",
        "unset",
        "acme/widget",
    ]);
    assert!(output.status.success(), "{output:?}");
    let unset = json_out(&output);
    assert_eq!(unset["data"]["removed"]["note"], "No AI PRs");
    assert_eq!(unset["meta"]["reminders"][0]["note"], "Org allows AI");
    let shown = json_out(&fixture.run(&["--json", "policy", "show", "acme/widget"]));
    assert!(shown["data"]["mark"].is_null());
    let history = shown["data"]["history"].as_array().unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history[1]["action"], "unset");
    let output = fixture.run(&[
        "--json",
        "--agent",
        "agent-a",
        "policy",
        "unset",
        "acme/widget",
    ]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
}

#[test]
fn quota_and_cache_commands() {
    let fixture = Fixture::new("quota-cache");
    let output = fixture.run(&["--json", "quota"]);
    assert!(output.status.success(), "{output:?}");
    let envelope = json_out(&output);
    assert_eq!(envelope["data"]["windows"]["per_minute"]["used"], 0);
    assert_eq!(envelope["data"]["windows"]["per_minute"]["limit"], 60);
    assert_eq!(envelope["data"]["resources"].as_array().unwrap().len(), 3);
    let human = stdout(&fixture.run(&["quota"]));
    assert!(human.contains("RESOURCE"), "{human}");
    assert!(human.contains("All-agent windows"), "{human}");

    seed_policy_files(&fixture);
    let output = fixture.run(&["--json", "cache", "stats"]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(json_out(&output)["data"]["rows"]["policy_files"], 4);

    let output = fixture.run(&["--json", "cache", "prune"]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(json_out(&output)["data"]["total"], 0);
    let human = stdout(&fixture.run(&["cache", "prune", "--older-than", "7d"]));
    assert!(human.contains("Deleted 0 row(s)"), "{human}");
}
