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
        &["--json", "policy", "files", "a/b"],
        &[
            "--json",
            "policy",
            "set",
            "a",
            "--stance",
            "bans",
            "--evidence",
            "README.md:1-2",
            "--note",
            "n",
        ],
        &["--json", "issue", "show", "a/b#1"],
        &["--json", "gate", "start", "https://github.com/a/b/issues/1"],
        &["--json", "gate", "pr", "a/b#1", "--branch", "x"],
        &["--json", "claim", "list"],
        &["--json", "track", "list"],
        &["--json", "sync", "--all-tracked"],
        &["--json", "quota"],
        &["--json", "cache", "prune", "--older-than", "7d"],
    ];
    for args in stubs {
        let output = fixture.run(args);
        assert_eq!(output.status.code(), Some(1), "{args:?}: {output:?}");
        let envelope: Value = serde_json::from_str(stdout(&output).trim()).unwrap();
        assert_eq!(envelope["schema"], "contrib-scout/v1", "{args:?}");
        assert_eq!(envelope["error"]["code"], "not_implemented", "{args:?}");
        assert!(envelope["meta"]["as_of"].is_string());
    }
    let human = fixture.run(&["quota"]);
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
