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
        &["--json", "discover"],
        &["--json", "repo", "show", "a/b"],
        &["--json", "candidates"],
        &["--json", "issue", "show", "a/b#1"],
        &["--json", "gate", "start", "https://github.com/a/b/issues/1"],
        &["--json", "gate", "pr", "a/b#1", "--branch", "x"],
        &["--json", "claim", "list"],
        &["--json", "track", "list"],
        &["--json", "sync", "--all-tracked"],
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
