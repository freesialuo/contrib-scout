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
        &["--json", "candidates"],
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
