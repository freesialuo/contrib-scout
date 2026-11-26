//! Live gates over FixtureTransport (§8, §13): pass path, every block reason,
//! fail closed on network errors, 304 as live confirmation, claims, gate_log.

use chrono::{TimeDelta, Utc};
use contrib_scout_core::config::Config;
use contrib_scout_core::coordinator::SqliteCoordinator;
use contrib_scout_core::gate::{
    looks_like_claim, pull_api_path, Gatekeeper, LiveGatekeeper, PrOptions,
};
use contrib_scout_core::gateway::{Budget, Gateway, HostPolicy};
use contrib_scout_core::issue_marks::{mark_issue, IssueVerdict, NewIssueMark, SkipReason};
use contrib_scout_core::models::{
    BlockReason, ClaimStatus, Evidence, GateKind, GateReport, GateVerdict, ObservationKind,
    PolicyMark, PolicyScope, Requirement, Restriction, Stance,
};
use contrib_scout_core::policy::CANDIDATE_PATHS;
use contrib_scout_core::store::{SharedStore, Store};
use contrib_scout_core::transport::fixture::Recorded;
use contrib_scout_core::transport::{ApiResponse, FixtureTransport};
use contrib_scout_core::{AgentId, IssueRef, MarkTarget, RepoName, ScoutError};
use serde_json::{json, Map, Value};
use std::sync::{Arc, PoisonError};

const ISSUE: &str = "/repos/o/r/issues/7";
const COMMENTS: &str = "/repos/o/r/issues/7/comments?per_page=20&page=1";
const TIMELINE: &str = "/repos/o/r/issues/7/timeline?per_page=100";
const ORG_HEAD: &str = "/repos/o/.github/commits/HEAD";
const REPO_HEAD: &str = "/repos/o/r/commits/HEAD";
const SHA: &str = "4444444444444444444444444444444444444444";
const CONTRIBUTING: &str = "# Contributing\nAI-assisted PRs must say so in the description.\n";
const TEMPLATE: &str = "## Summary\n\n- [ ] I disclosed any AI assistance\n";

struct Harness {
    fixture: Arc<FixtureTransport>,
    gateway: Arc<Gateway>,
    store: SharedStore,
    gate: LiveGatekeeper,
}

fn harness() -> Harness {
    let store = Store::memory().unwrap().shared();
    let fixture = Arc::new(FixtureTransport::new());
    let policy = HostPolicy {
        max_attempts: 1,
        ..HostPolicy::default()
    };
    let gateway = Arc::new(
        Gateway::new(fixture.clone(), policy, Budget::new(200))
            .with_cache(store.clone())
            .with_sleep(Arc::new(|_| {})),
    );
    let config = Config::default();
    let coordinator = Arc::new(SqliteCoordinator::from_config(
        store.clone(),
        agent("a1"),
        &config,
    ));
    let gate = LiveGatekeeper::new(gateway.clone(), store.clone(), config, coordinator);
    Harness {
        fixture,
        gateway,
        store,
        gate,
    }
}

fn agent(id: &str) -> AgentId {
    id.parse().unwrap()
}

fn repo() -> RepoName {
    "o/r".parse().unwrap()
}

fn issue_ref() -> IssueRef {
    IssueRef::new(repo(), 7)
}

fn store(h: &Harness) -> std::sync::MutexGuard<'_, Store> {
    h.store.lock().unwrap_or_else(PoisonError::into_inner)
}

fn put_mark(h: &Harness, edit: impl FnOnce(&mut PolicyMark)) {
    let now = Utc::now();
    let mut mark = PolicyMark {
        target: MarkTarget::Repo(repo()),
        stance: Stance::Disclose,
        scope: PolicyScope::default(),
        requirements: vec![Requirement::DiscloseInPr, Requirement::AssistedByTrailer],
        evidence: vec![Evidence {
            path: "CONTRIBUTING.md".into(),
            lines: None,
            url: format!("https://github.com/o/r/blob/{SHA}/CONTRIBUTING.md#L2"),
            excerpt: "AI-assisted PRs must say so".into(),
        }],
        note: "AI allowed if disclosed".into(),
        marked_by: agent("a1"),
        marked_at: now,
        expires_at: now + TimeDelta::days(30),
    };
    edit(&mut mark);
    store(h).put_mark(&mark).unwrap();
}

/// What GitHub says in one gate run.
#[derive(Clone)]
struct Spec {
    viewer: &'static str,
    archived: bool,
    state: &'static str,
    labels: Vec<&'static str>,
    assignees: Vec<&'static str>,
    comments: Vec<Value>,
    timeline: Vec<Value>,
    files: bool,
    pulls: Vec<(u64, Value)>,
}

impl Default for Spec {
    fn default() -> Self {
        Self {
            viewer: "me",
            archived: false,
            state: "open",
            labels: vec!["bug"],
            assignees: Vec::new(),
            comments: Vec::new(),
            timeline: Vec::new(),
            files: false,
            pulls: Vec::new(),
        }
    }
}

fn comment(id: u32, login: &str, association: &str, body: &str) -> Value {
    json!({
        "user": {"login": login},
        "author_association": association,
        "body": body,
        "created_at": "2026-11-20T00:00:00Z",
        "html_url": format!("https://github.com/o/r/issues/7#issuecomment-{id}"),
    })
}

fn linked_pr(number: u64, author: &str, state: &str) -> Value {
    json!({
        "event": "cross-referenced",
        "source": {"issue": {
            "number": number,
            "state": state,
            "user": {"login": author},
            "html_url": format!("https://github.com/o/r/pull/{number}"),
            "pull_request": {
                "html_url": format!("https://github.com/o/r/pull/{number}"),
                "merged_at": Value::Null,
            },
        }},
    })
}

fn pull(number: u64, author: &str, head: &str) -> (u64, Value) {
    (
        number,
        json!({"number": number, "user": {"login": author}, "head": {"ref": head}}),
    )
}

fn issue_json(spec: &Spec) -> Value {
    json!({
        "number": 7,
        "title": "Widget crashes",
        "state": spec.state,
        "labels": spec.labels.iter().map(|l| json!({"name": l})).collect::<Vec<_>>(),
        "assignees": spec.assignees.iter().map(|a| json!({"login": a})).collect::<Vec<_>>(),
        "comments": spec.comments.len(),
        "created_at": "2026-11-01T00:00:00Z",
        "updated_at": "2026-11-20T00:00:00Z",
        "user": {"login": "reporter"},
        "body": "Steps to reproduce: run it.",
        "html_url": "https://github.com/o/r/issues/7",
    })
}

fn listing() -> Value {
    let entry = |name: &str, oid: &str, size: usize| json!({"name": name, "type": "blob", "oid": oid, "object": {"byteSize": size}});
    let blob = |oid: &str, text: &str| json!({"oid": oid, "byteSize": text.len(), "isTruncated": false, "text": text});
    let mut repo = Map::new();
    repo.insert(
        "d0".into(),
        json!({"entries": [entry("CONTRIBUTING.md", "c1", CONTRIBUTING.len())]}),
    );
    repo.insert(
        "d1".into(),
        json!({"entries": [entry("PULL_REQUEST_TEMPLATE.md", "t1", TEMPLATE.len())]}),
    );
    repo.insert("d2".into(), Value::Null);
    for (j, path) in CANDIDATE_PATHS.iter().enumerate() {
        let text = match *path {
            "CONTRIBUTING.md" => blob("c1", CONTRIBUTING),
            ".github/PULL_REQUEST_TEMPLATE.md" => blob("t1", TEMPLATE),
            _ => Value::Null,
        };
        repo.insert(format!("f{j}"), text);
    }
    json!({"data": {"s0": Value::Object(repo)}})
}

fn not_found() -> ApiResponse {
    ApiResponse::json(404, &json!({"message": "Not Found"}))
}

/// Scripts one gate run in request order.
fn script(fixture: &FixtureTransport, spec: &Spec) {
    fixture.expect_get(
        "/user",
        ApiResponse::json(200, &json!({"login": spec.viewer})),
    );
    fixture.expect_get(
        "/repos/o/r",
        ApiResponse::json(
            200,
            &json!({"full_name": "o/r", "archived": spec.archived, "fork": false, "default_branch": "main"}),
        )
        .with_etag("\"repo\""),
    );
    fixture.expect_get(
        ISSUE,
        ApiResponse::json(200, &issue_json(spec)).with_etag("\"issue\""),
    );
    if !spec.comments.is_empty() {
        fixture.expect_get(
            COMMENTS,
            ApiResponse::json(200, &Value::Array(spec.comments.clone())).with_etag("\"comments\""),
        );
    }
    fixture.expect_get(
        TIMELINE,
        ApiResponse::json(200, &Value::Array(spec.timeline.clone())).with_etag("\"timeline\""),
    );
    fixture.expect_get(ORG_HEAD, not_found());
    if spec.files {
        fixture.expect_get(REPO_HEAD, ApiResponse::new(200, SHA).with_etag("\"head\""));
        fixture.expect_graphql(ApiResponse::json(200, &listing()));
    } else {
        fixture.expect_get(REPO_HEAD, not_found());
    }
    for (number, body) in &spec.pulls {
        fixture.expect_get(
            format!("/repos/o/r/pulls/{number}"),
            ApiResponse::json(200, body),
        );
    }
}

fn start(h: &Harness, spec: &Spec, who: &str) -> GateReport {
    script(&h.fixture, spec);
    let report = h.gate.start(&issue_ref(), &agent(who)).unwrap();
    h.fixture.assert_drained();
    report
}

fn pr(h: &Harness, spec: &Spec, who: &str, options: &PrOptions) -> GateReport {
    script(&h.fixture, spec);
    let report = h.gate.pr(&issue_ref(), &agent(who), options).unwrap();
    h.fixture.assert_drained();
    report
}

fn reasons(report: &GateReport) -> Vec<BlockReason> {
    report.blocked_by.iter().map(|f| f.reason.clone()).collect()
}

fn kinds(report: &GateReport) -> Vec<ObservationKind> {
    report.observations.iter().map(|o| o.kind.clone()).collect()
}

fn branch(name: &str) -> PrOptions {
    PrOptions {
        branch: Some(name.into()),
        approval_url: None,
    }
}

#[test]
fn start_passes_acquires_a_claim_and_logs() {
    let h = harness();
    put_mark(&h, |_| {});
    let report = start(&h, &Spec::default(), "a1");
    assert_eq!(report.verdict, GateVerdict::Pass, "{:?}", report.blocked_by);
    assert_eq!(report.kind, GateKind::Start);
    assert_eq!(report.viewer.as_deref(), Some("me"));
    let claim = report.claim.as_ref().expect("claim acquired");
    assert_eq!(claim.agent, agent("a1"));
    assert_eq!(claim.status, ClaimStatus::Active);
    // Requirements preview; the checklist is a `gate pr` thing.
    assert_eq!(
        report.requirements,
        [Requirement::DiscloseInPr, Requirement::AssistedByTrailer]
    );
    assert!(report.checklist.is_empty());
    let raw = &report.raw;
    assert_eq!(
        raw.issue.as_ref().unwrap().body.as_deref(),
        Some("Steps to reproduce: run it.")
    );
    assert_eq!(
        raw.policy_mark.as_ref().unwrap().note,
        "AI allowed if disclosed"
    );
    // user, repo, issue, timeline, two policy HEADs: a bounded number of live reads.
    assert_eq!(h.fixture.request_count(), 6);
    assert_eq!(h.gateway.metrics().requests, 6);

    let log = store(&h).gate_reports(Some(&issue_ref())).unwrap();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].verdict, GateVerdict::Pass);
    assert_eq!(log[0].claim, report.claim);
}

#[test]
fn second_agent_is_blocked_and_pr_needs_the_claim() {
    let h = harness();
    put_mark(&h, |_| {});
    assert_eq!(start(&h, &Spec::default(), "a1").verdict, GateVerdict::Pass);

    let other = start(&h, &Spec::default(), "a2");
    assert_eq!(reasons(&other), [BlockReason::ClaimedByOther]);
    assert!(other.claim.is_none());

    let other_pr = pr(&h, &Spec::default(), "a2", &PrOptions::default());
    assert_eq!(reasons(&other_pr), [BlockReason::ClaimedByOther]);

    h.gate.claims().release(&issue_ref(), &agent("a1")).unwrap();
    let nobody = pr(&h, &Spec::default(), "a1", &PrOptions::default());
    assert_eq!(reasons(&nobody), [BlockReason::ClaimNotHeld]);
}

#[test]
fn blocked_start_does_not_acquire_a_claim() {
    let h = harness();
    let report = start(&h, &Spec::default(), "a1");
    assert_eq!(report.verdict, GateVerdict::Blocked);
    assert_eq!(reasons(&report), [BlockReason::PolicyUnmarked]);
    assert!(report.claim.is_none());
    assert!(h.gate.claims().list(None).unwrap().is_empty());
}

#[test]
fn each_block_reason() {
    type Setup = fn(&Harness, &mut Spec);
    let cases: Vec<(BlockReason, Setup)> = vec![
        (BlockReason::IssueClosed, |_, s| s.state = "closed"),
        (BlockReason::AssignedToOther, |_, s| {
            s.assignees = vec!["someone"]
        }),
        (BlockReason::PrExists, |_, s| {
            s.timeline = vec![linked_pr(8, "someone", "open")]
        }),
        (BlockReason::RepoArchived, |_, s| s.archived = true),
        (BlockReason::PolicyBans, |h, _| {
            put_mark(h, |m| m.stance = Stance::Bans)
        }),
        (BlockReason::ForbiddenLabel, |h, s| {
            s.labels = vec!["Good First Issue"];
            put_mark(h, |m| {
                m.scope.forbidden_labels = vec!["good first issue".into()]
            });
        }),
        (BlockReason::IssueSkipped, |h, _| {
            let new = NewIssueMark {
                issue: issue_ref(),
                verdict: IssueVerdict::Skip,
                reason: Some(SkipReason::TooLarge),
                note: "Needs a redesign".into(),
                expires_at: None,
            };
            mark_issue(&mut store(h), new, &agent("a3"), Utc::now()).unwrap();
        }),
    ];
    for (expected, setup) in cases {
        let h = harness();
        put_mark(&h, |_| {});
        let mut spec = Spec::default();
        setup(&h, &mut spec);
        let report = start(&h, &spec, "a1");
        assert_eq!(
            reasons(&report),
            std::slice::from_ref(&expected),
            "{expected}"
        );
        assert_eq!(report.verdict, GateVerdict::Blocked);
        assert!(report.claim.is_none(), "{expected}");
        let logged = store(&h).gate_reports(None).unwrap();
        assert_eq!(logged.last().unwrap().blocked_by, report.blocked_by);
    }
}

#[test]
fn skip_blocks_pr_too_and_shortlist_only_observes() {
    let h = harness();
    put_mark(&h, |_| {});
    assert_eq!(start(&h, &Spec::default(), "a1").verdict, GateVerdict::Pass);
    let mut new = NewIssueMark {
        issue: issue_ref(),
        verdict: IssueVerdict::Skip,
        reason: Some(SkipReason::Duplicate),
        note: "Same as #3".into(),
        expires_at: None,
    };
    mark_issue(&mut store(&h), new.clone(), &agent("a2"), Utc::now()).unwrap();
    let report = pr(&h, &Spec::default(), "a1", &PrOptions::default());
    assert_eq!(reasons(&report), [BlockReason::IssueSkipped]);
    assert!(report.blocked_by[0].detail.contains("Same as #3"));

    new.verdict = IssueVerdict::Shortlist;
    new.reason = None;
    mark_issue(&mut store(&h), new, &agent("a2"), Utc::now()).unwrap();
    let report = pr(&h, &Spec::default(), "a1", &PrOptions::default());
    assert_eq!(report.verdict, GateVerdict::Pass, "{:?}", report.blocked_by);
    assert!(kinds(&report).contains(&ObservationKind::IssueMark));
}

#[test]
fn assignment_to_the_viewer_and_comment_claims_are_reported_not_blocking() {
    let h = harness();
    put_mark(&h, |_| {});
    let spec = Spec {
        assignees: vec!["Me"],
        comments: vec![
            comment(
                1,
                "bob",
                "NONE",
                "I'd like to work on this, can you assign me?",
            ),
            comment(2, "carol", "NONE", "Same problem here."),
        ],
        timeline: vec![linked_pr(5, "dave", "closed")],
        ..Spec::default()
    };
    let report = start(&h, &spec, "a1");
    assert_eq!(report.verdict, GateVerdict::Pass, "{:?}", report.blocked_by);
    assert_eq!(
        kinds(&report),
        [
            ObservationKind::AssignedToYou,
            ObservationKind::ClosedPr,
            ObservationKind::CommentClaim
        ]
    );
    let claim = &report.observations[2];
    assert_eq!(claim.severity.as_str(), "info");
    assert_eq!(
        claim.url.as_deref(),
        Some("https://github.com/o/r/issues/7#issuecomment-1")
    );
    assert_eq!(report.raw.issue.as_ref().unwrap().comments.len(), 2);
}

#[test]
fn pr_returns_full_policy_text_requirements_and_excludes_own_branch() {
    let h = harness();
    put_mark(&h, |m| {
        m.requirements.push(Requirement::LabelPr("ai".into()))
    });
    assert_eq!(start(&h, &Spec::default(), "a1").verdict, GateVerdict::Pass);

    let spec = Spec {
        files: true,
        timeline: vec![linked_pr(8, "me", "open")],
        pulls: vec![pull(8, "me", "fix-7")],
        ..Spec::default()
    };
    let report = pr(&h, &spec, "a1", &branch("fix-7"));
    assert_eq!(report.verdict, GateVerdict::Pass, "{:?}", report.blocked_by);
    assert!(kinds(&report).contains(&ObservationKind::OwnPrExcluded));
    let linked = &report.raw.issue.as_ref().unwrap().linked_prs[0];
    assert_eq!(linked.head_ref.as_deref(), Some("fix-7"));

    // Full current text of every candidate file.
    let texts: Vec<(&str, Option<&str>)> = report
        .raw
        .policy_files
        .iter()
        .map(|f| (f.path.as_str(), f.text.as_deref()))
        .collect();
    assert_eq!(
        texts,
        [
            ("CONTRIBUTING.md", Some(CONTRIBUTING)),
            (".github/PULL_REQUEST_TEMPLATE.md", Some(TEMPLATE)),
        ]
    );
    assert_eq!(
        report.requirements,
        [
            Requirement::DiscloseInPr,
            Requirement::AssistedByTrailer,
            Requirement::LabelPr("ai".into())
        ]
    );
    assert!(report.checklist.iter().any(|l| l.contains("Assisted-by:")));
    assert!(report
        .checklist
        .iter()
        .any(|l| l.starts_with("Follow the PR template o/r/.github/PULL_REQUEST_TEMPLATE.md")));
    assert_eq!(
        report.claim.as_ref().unwrap().branch.as_deref(),
        Some("fix-7")
    );
    let pulls: Vec<_> = h
        .fixture
        .requests()
        .iter()
        .filter_map(Recorded::path)
        .filter(|p| p.contains("/pulls/"))
        .map(str::to_owned)
        .collect();
    assert_eq!(pulls, ["/repos/o/r/pulls/8"]);
}

#[test]
fn pr_with_another_branch_still_sees_the_pr() {
    let h = harness();
    put_mark(&h, |_| {});
    assert_eq!(start(&h, &Spec::default(), "a1").verdict, GateVerdict::Pass);
    let spec = Spec {
        timeline: vec![linked_pr(8, "me", "open"), linked_pr(9, "someone", "open")],
        pulls: vec![pull(8, "me", "other-work")],
        ..Spec::default()
    };
    let report = pr(&h, &spec, "a1", &branch("fix-7"));
    assert_eq!(
        reasons(&report),
        [BlockReason::PrExists, BlockReason::PrExists]
    );

    // Without --branch, nothing is looked up and the PR blocks.
    let spec = Spec {
        timeline: vec![linked_pr(8, "me", "open")],
        ..Spec::default()
    };
    let report = pr(&h, &spec, "a1", &PrOptions::default());
    assert_eq!(reasons(&report), [BlockReason::PrExists]);
}

#[test]
fn pr_blocks_on_human_required_and_missing_approval() {
    let h = harness();
    put_mark(&h, |m| {
        m.scope.pr_text = Restriction::HumanWritten;
        m.scope.needs_prior_approval = true;
    });
    let start_report = start(&h, &Spec::default(), "a1");
    // Approval is only reported on start; human_required is a `gate pr` verdict.
    assert_eq!(start_report.verdict, GateVerdict::Pass);
    assert!(kinds(&start_report).contains(&ObservationKind::ApprovalRequired));

    let spec = Spec {
        comments: vec![comment(4, "lead", "MEMBER", "Sounds good, go ahead.")],
        ..Spec::default()
    };
    let report = pr(&h, &spec, "a1", &PrOptions::default());
    assert_eq!(
        reasons(&report),
        [BlockReason::ApprovalMissing, BlockReason::HumanRequired]
    );
    assert!(kinds(&report).contains(&ObservationKind::ApprovalEvidence));

    let options = PrOptions {
        branch: None,
        approval_url: Some("https://github.com/o/r/issues/7#issuecomment-4".into()),
    };
    let report = pr(&h, &spec, "a1", &options);
    assert_eq!(reasons(&report), [BlockReason::HumanRequired]);
    assert!(report
        .checklist
        .iter()
        .any(|l| l == "Maintainer approval: https://github.com/o/r/issues/7#issuecomment-4"));

    put_mark(&h, |m| m.scope.autonomous_agents = Restriction::Forbidden);
    let report = pr(&h, &Spec::default(), "a1", &PrOptions::default());
    assert_eq!(reasons(&report), [BlockReason::HumanRequired]);
    assert!(report.blocked_by[0].detail.contains("autonomous_agents"));
}

#[test]
fn network_down_fails_closed_even_with_a_warm_cache() {
    let h = harness();
    put_mark(&h, |_| {});
    assert_eq!(start(&h, &Spec::default(), "a1").verdict, GateVerdict::Pass);
    let claim_before = h.gate.claims().list(None).unwrap();

    h.fixture
        .expect_get("/user", ApiResponse::json(200, &json!({"login": "me"})));
    h.fixture
        .expect_get("/repos/o/r", ApiResponse::not_modified());
    h.fixture.expect_network_down(ISSUE);
    let error = h.gate.start(&issue_ref(), &agent("a1")).unwrap_err();
    h.fixture.assert_drained();
    assert!(matches!(
        ScoutError::find(&error),
        Some(ScoutError::NetworkUnavailable { .. })
    ));
    assert!(format!("{error:#}").contains("failed closed"));

    let log = store(&h).gate_reports(Some(&issue_ref())).unwrap();
    assert_eq!(log.len(), 2);
    assert_eq!(log[1].verdict, GateVerdict::Error);
    assert!(log[1].error.as_deref().unwrap().contains("network"));
    // The lease was not renewed by the failed gate.
    assert_eq!(h.gate.claims().list(None).unwrap(), claim_before);

    // `/user` failing is an error too.
    h.fixture.expect_network_down("/user");
    assert!(h.gate.start(&issue_ref(), &agent("a1")).is_err());
}

#[test]
fn not_modified_counts_as_live_confirmation() {
    let h = harness();
    put_mark(&h, |_| {});
    let first = start(&h, &Spec::default(), "a1");
    assert_eq!(first.verdict, GateVerdict::Pass);

    h.fixture
        .expect_get("/user", ApiResponse::json(200, &json!({"login": "me"})));
    for path in ["/repos/o/r", ISSUE, TIMELINE] {
        h.fixture.expect_get(path, ApiResponse::not_modified());
    }
    h.fixture.expect_get(ORG_HEAD, not_found());
    h.fixture.expect_get(REPO_HEAD, not_found());
    let before = h.gateway.metrics();
    let report = h.gate.start(&issue_ref(), &agent("a1")).unwrap();
    h.fixture.assert_drained();
    assert_eq!(report.verdict, GateVerdict::Pass, "{:?}", report.blocked_by);
    assert_eq!(
        report.raw.issue.as_ref().unwrap().row.title,
        "Widget crashes"
    );
    let after = h.gateway.metrics();
    assert_eq!(after.requests - before.requests, 6);
    assert_eq!(after.served_304 - before.served_304, 3);
    // Revalidation, not a cache read: the conditional requests went out.
    let revalidated = h.fixture.requests()[7..10]
        .iter()
        .all(|r| matches!(r, Recorded::Get(req) if req.if_none_match.is_some()));
    assert!(revalidated);
    // Renewed, not re-acquired.
    let claim = report.claim.unwrap();
    assert_eq!(claim.claimed_at, first.claim.unwrap().claimed_at);
}

#[test]
fn helpers() {
    assert!(looks_like_claim("Hi! I\u{2019}ll take this one."));
    assert!(looks_like_claim("/assign"));
    assert!(!looks_like_claim("This also happens on Windows."));
    assert_eq!(
        pull_api_path("https://github.com/o/r/pull/12").as_deref(),
        Some("/repos/o/r/pulls/12")
    );
    assert_eq!(pull_api_path("https://github.com/o/r/issues/12"), None);
    assert_eq!(pull_api_path("https://evil.example/o/r/pull/12"), None);
}
