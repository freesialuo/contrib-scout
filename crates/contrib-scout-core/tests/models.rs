use chrono::{TimeZone, Utc};
use contrib_scout_core::models::*;
use serde_json::{json, Value};

fn repo(s: &str) -> RepoName {
    s.parse().unwrap()
}

#[test]
fn repo_name_parsing() {
    let r = repo("rust-lang/rust");
    assert_eq!((r.owner(), r.name()), ("rust-lang", "rust"));
    assert_eq!(
        repo("https://github.com/astral-sh/uv.git").to_string(),
        "astral-sh/uv"
    );
    assert_eq!(repo("github.com/astral-sh/uv/").to_string(), "astral-sh/uv");
    assert_eq!(repo("o/.github").name(), ".github");
    assert_eq!(repo("Owner/Repo").key(), "owner/repo");
    for bad in [
        "",
        "rust",
        "a/b/c",
        "-x/y",
        "x/..",
        "x/y z",
        "https://gitlab.com/a/b",
    ] {
        assert!(
            bad.parse::<RepoName>().is_err(),
            "{bad:?} should be rejected"
        );
    }
    assert_eq!(
        repo("astral-sh/ruff").org_policy_repo().to_string(),
        "astral-sh/.github"
    );
}

#[test]
fn issue_ref_parsing() {
    let expected = IssueRef::new(repo("pola-rs/polars"), 123);
    for input in [
        "pola-rs/polars#123",
        "https://github.com/pola-rs/polars/issues/123",
        "https://github.com/pola-rs/polars/issues/123/",
        "https://github.com/pola-rs/polars/issues/123#issuecomment-9",
        "github.com/pola-rs/polars/issues/123?x=1",
    ] {
        assert_eq!(input.parse::<IssueRef>().unwrap(), expected, "{input}");
    }
    assert_eq!(expected.to_string(), "pola-rs/polars#123");
    assert_eq!(
        expected.html_url(),
        "https://github.com/pola-rs/polars/issues/123"
    );
    for bad in [
        "pola-rs/polars",
        "pola-rs/polars#0",
        "pola-rs/polars#-1",
        "pola-rs/polars#x",
        "https://github.com/pola-rs/polars/pull/5",
    ] {
        assert!(
            bad.parse::<IssueRef>().is_err(),
            "{bad:?} should be rejected"
        );
    }
}

#[test]
fn mark_targets_and_agents() {
    assert_eq!(
        "astral-sh".parse::<MarkTarget>().unwrap(),
        MarkTarget::Org("astral-sh".parse().unwrap())
    );
    assert_eq!(
        "astral-sh/uv".parse::<MarkTarget>().unwrap(),
        MarkTarget::Repo(repo("astral-sh/uv"))
    );
    assert_eq!(
        serde_json::to_value(MarkTarget::Repo(repo("a/b"))).unwrap(),
        json!({"kind": "repo", "name": "a/b"})
    );
    assert!("agent-1".parse::<AgentId>().is_ok());
    assert!("has space".parse::<AgentId>().is_err());
    assert!("".parse::<AgentId>().is_err());
}

#[test]
fn cli_value_parsers() {
    assert_eq!("none-found".parse::<Stance>().unwrap(), Stance::NoneFound);
    assert!("maybe".parse::<Stance>().is_err());
    assert_eq!(
        "label_pr=ai-assisted".parse::<Requirement>().unwrap(),
        Requirement::LabelPr("ai-assisted".into())
    );
    assert_eq!(
        "assisted-by-trailer".parse::<Requirement>().unwrap(),
        Requirement::AssistedByTrailer
    );
    assert!("label_pr".parse::<Requirement>().is_err());

    let spec: EvidenceSpec = "CONTRIBUTING.md:10-12".parse().unwrap();
    assert_eq!(spec.path, "CONTRIBUTING.md");
    assert_eq!(spec.lines, Some(LineRange { start: 10, end: 12 }));
    let spec: EvidenceSpec = "https://ziglang.org/code-of-conduct/".parse().unwrap();
    assert_eq!(
        (spec.path.as_str(), spec.lines),
        ("https://ziglang.org/code-of-conduct/", None)
    );
    assert!("a.md:5-2".parse::<EvidenceSpec>().is_err());

    let mut scope = PolicyScope::default();
    scope
        .apply(&"pr_text=human-written".parse().unwrap())
        .unwrap();
    scope
        .apply(&"forbidden_label=good first issue".parse().unwrap())
        .unwrap();
    scope
        .apply(&"needs_prior_approval=true".parse().unwrap())
        .unwrap();
    assert!(scope.requires_human());
    assert_eq!(scope.forbidden_labels, ["good first issue"]);
    assert!(scope.needs_prior_approval);
    assert!("bogus=1".parse::<ScopeSetting>().is_err());
    assert!("code=sometimes".parse::<ScopeSetting>().is_err());
}

fn sample_mark() -> PolicyMark {
    let at = Utc.with_ymd_and_hms(2026, 11, 20, 0, 0, 0).unwrap();
    PolicyMark {
        target: MarkTarget::Org("astral-sh".parse().unwrap()),
        stance: Stance::Restricts,
        scope: PolicyScope {
            pr_text: Restriction::HumanWritten,
            forbidden_labels: vec!["good first issue".into()],
            ..PolicyScope::default()
        },
        requirements: vec![Requirement::DiscloseInPr, Requirement::LabelPr("ai".into())],
        evidence: vec![Evidence {
            path: "CONTRIBUTING.md".into(),
            lines: Some(LineRange { start: 3, end: 4 }),
            url: "https://github.com/astral-sh/.github/blob/abc/CONTRIBUTING.md#L3-L4".into(),
            excerpt: "line one\nline two".into(),
        }],
        note: "AI allowed with disclosure; PR text by humans".into(),
        marked_by: "agent-1".parse().unwrap(),
        marked_at: at,
        expires_at: at + chrono::TimeDelta::days(30),
    }
}

#[test]
fn policy_mark_roundtrip_and_validation() {
    let mark = sample_mark();
    let value = serde_json::to_value(&mark).unwrap();
    assert_eq!(value["stance"], "restricts");
    assert_eq!(value["scope"]["pr_text"], "human_written");
    assert_eq!(
        value["requirements"],
        json!(["disclose_in_pr", {"label_pr": "ai"}])
    );
    let back: PolicyMark = serde_json::from_value(value).unwrap();
    assert_eq!(back, mark);
    mark.validate().unwrap();
    assert!(!mark.review_due(mark.marked_at));
    assert!(mark.review_due(mark.expires_at));

    let mut no_evidence = mark.clone();
    no_evidence.evidence.clear();
    assert!(no_evidence.validate().is_err());
    let mut long = mark.clone();
    long.evidence[0].excerpt = "1\n2\n3\n4".into();
    assert!(long.validate().is_err());
    let mut blank = mark;
    blank.note = "  ".into();
    assert!(blank.validate().is_err());
}

#[test]
fn unknown_values_are_preserved() {
    let mut value = serde_json::to_value(sample_mark()).unwrap();
    value["stance"] = json!("tolerates");
    value["scope"]["code"] = json!("sometimes");
    value["requirements"] = json!(["sign_cla", {"future": 1}]);
    let mark: PolicyMark = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(mark.stance, Stance::Unknown("tolerates".into()));
    assert_eq!(mark.scope.code, Restriction::Unknown("sometimes".into()));
    assert_eq!(
        mark.requirements,
        [
            Requirement::Unknown(json!("sign_cla")),
            Requirement::Unknown(json!({"future": 1}))
        ]
    );
    let again: Value = serde_json::to_value(&mark).unwrap();
    assert_eq!(again, value);
    assert_eq!(
        serde_json::from_value::<BlockReason>(json!("cla_missing")).unwrap(),
        BlockReason::Unknown("cla_missing".into())
    );
}

#[test]
fn gate_and_claim_shapes() {
    assert_eq!(GateVerdict::Pass.exit_code(), 0);
    assert_eq!(GateVerdict::Blocked.exit_code(), 20);
    assert_eq!(GateVerdict::Error.exit_code(), 1);
    assert_eq!(
        serde_json::to_value(BlockReason::PolicyUnmarked).unwrap(),
        "policy_unmarked"
    );
    let at = Utc.with_ymd_and_hms(2026, 11, 25, 0, 0, 0).unwrap();
    let outcome = ClaimOutcome::HeldByOther {
        claim: Claim {
            issue: "a/b#1".parse().unwrap(),
            agent: "other".parse().unwrap(),
            claimed_at: at,
            lease_until: at + chrono::TimeDelta::hours(48),
            status: ClaimStatus::Active,
            branch: None,
        },
    };
    let value = serde_json::to_value(&outcome).unwrap();
    assert_eq!(value["outcome"], "held_by_other");
    assert_eq!(value["claim"]["issue"], "a/b#1");
    assert_eq!(
        serde_json::from_value::<ClaimOutcome>(value).unwrap(),
        outcome
    );
}

#[test]
fn reminders() {
    use contrib_scout_core::policy::{human_line, reminder_for};
    let mark = sample_mark();
    let r = repo("astral-sh/uv");
    let reminder = reminder_for(&r, Some(&mark), mark.marked_at);
    assert_eq!(reminder.kind, ReminderKind::Marked);
    assert!(!reminder.review_due);
    let line = human_line(&reminder);
    assert!(
        line.starts_with("⚠ astral-sh/uv: Restricts AI — \""),
        "{line}"
    );
    assert!(
        line.contains("(marked 2026-11-20)") && line.contains("[via org astral-sh]"),
        "{line}"
    );
    assert!(reminder_for(&r, Some(&mark), mark.expires_at).review_due);

    let unmarked = reminder_for(&r, None, mark.marked_at);
    assert_eq!(unmarked.kind, ReminderKind::Unmarked);
    assert!(unmarked.hint.unwrap().contains("policy files astral-sh/uv"));
}

#[test]
fn labels_normalise_case_separators_and_spaces() {
    use contrib_scout_core::models::normalize_label;
    assert_eq!(normalize_label("  Good-First_Issue  "), "good first issue");
    assert_eq!(normalize_label("good   first	issue"), "good first issue");
    assert_eq!(normalize_label("ÄRGER"), "ärger");
}
