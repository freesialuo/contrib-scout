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

fn usage_envelope(output: &Output) -> Value {
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let envelope: Value = serde_json::from_str(stdout(output).trim()).unwrap();
    assert_eq!(envelope["schema"], "contrib-scout/v1", "{envelope}");
    assert_eq!(envelope["data"], Value::Null, "{envelope}");
    assert_eq!(envelope["error"]["code"], "usage", "{envelope}");
    assert_eq!(envelope["error"]["exit_code"], 2, "{envelope}");
    envelope
}

#[test]
fn clap_errors_emit_the_error_envelope_in_json_modes() {
    let fixture = Fixture::new("clap-json");
    for args in [
        &["--json", "issue", "show", "owner/repo"][..],
        &["--ndjson", "issue", "show", "owner/repo"],
        &["issue", "show", "owner/repo", "--json"],
        &["--json", "frobnicate"],
        &["--json", "issue"],
        &["--ndjson", "issue"],
    ] {
        let output = fixture.run(args);
        usage_envelope(&output);
        assert!(!output.stderr.is_empty(), "{args:?}: {output:?}");
    }

    for args in [&["issue", "show", "owner/repo"][..], &["issue"]] {
        let output = fixture.run(args);
        assert_eq!(output.status.code(), Some(2), "{args:?}: {output:?}");
        assert!(output.stdout.is_empty(), "{args:?}: {output:?}");
        assert!(!output.stderr.is_empty(), "{args:?}: {output:?}");
    }

    // With no env-backed arguments set, clap reports a bare `issue` as
    // DisplayHelpOnMissingArgumentOrSubcommand and renders the whole help
    // page; the envelope carries a short message instead. Parsing fails
    // before any file is touched, so the default paths are never used.
    for flag in ["--json", "--ndjson"] {
        let output = Command::new(env!("CARGO_BIN_EXE_contrib-scout"))
            .current_dir(&fixture.root)
            .env_remove("CONTRIB_SCOUT_DB")
            .env_remove("CONTRIB_SCOUT_CONFIG")
            .env_remove("CONTRIB_SCOUT_AGENT")
            .args([flag, "issue"])
            .output()
            .unwrap();
        let envelope = usage_envelope(&output);
        assert_eq!(
            envelope["error"]["message"],
            "a subcommand or argument is missing; see --help"
        );
        assert!(!output.stderr.is_empty(), "{output:?}");
    }
}

#[test]
fn help_and_version_still_exit_zero() {
    let fixture = Fixture::new("clap-help");
    for args in [
        &["--help"][..],
        &["--version"],
        &["--json", "--help"],
        &["--json", "--version"],
        &["issue", "--help"],
    ] {
        let output = fixture.run(args);
        assert_eq!(output.status.code(), Some(0), "{args:?}: {output:?}");
        assert!(!output.stdout.is_empty(), "{args:?}: {output:?}");
    }
}

#[test]
fn policy_evidence_with_multibyte_path_does_not_panic() {
    let fixture = Fixture::new("policy-multibyte");
    let run = |evidence: &str| {
        fixture.run(&[
            "--json",
            "--offline",
            "--agent",
            "a",
            "policy",
            "set",
            "ab/repo",
            "--stance",
            "allows",
            "--evidence",
            evidence,
            "--note",
            "x",
        ])
    };
    let multibyte = run("贡献指南.md");
    assert_ne!(multibyte.status.code(), Some(101), "{multibyte:?}");
    let plain = run("guide.md");
    assert_eq!(
        multibyte.status.code(),
        plain.status.code(),
        "{multibyte:?}"
    );
    let envelope: Value = serde_json::from_str(stdout(&multibyte).trim()).unwrap();
    assert_eq!(envelope["schema"], "contrib-scout/v1", "{envelope}");
}

#[test]
fn issue_mark_until_overflow_is_a_usage_error() {
    let fixture = Fixture::new("until-overflow");
    for until in ["100000000d", "99999999999999999d"] {
        let output = fixture.run(&[
            "--json",
            "--agent",
            "a",
            "issue",
            "mark",
            "o/r#1",
            "--verdict",
            "shortlist",
            "--note",
            "n",
            "--until",
            until,
        ]);
        assert_ne!(output.status.code(), Some(101), "{until}: {output:?}");
        usage_envelope(&output);
    }
}

#[test]
fn candidates_min_score_must_be_finite_and_in_range() {
    let fixture = Fixture::new("min-score");
    for value in ["NaN", "inf", "-inf", "-1", "100.5", "abc"] {
        let output = fixture.run(&["candidates", "--min-score", value]);
        assert_eq!(output.status.code(), Some(2), "{value}: {output:?}");
    }
    for value in ["0", "55.5", "100"] {
        let output = fixture.run(&["--json", "candidates", "--min-score", value]);
        assert!(output.status.success(), "{value}: {output:?}");
    }
}
