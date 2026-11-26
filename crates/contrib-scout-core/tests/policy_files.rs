//! Raw policy file fetching over FixtureTransport (§5 step 4, §7, §8, §13).

use contrib_scout_core::config::Config;
use contrib_scout_core::freshness::Origin;
use contrib_scout_core::gateway::{Budget, Gateway, HostPolicy};
use contrib_scout_core::policy::files::{pinned_url, SHA_MEDIA_TYPE};
use contrib_scout_core::policy::{GitHubPolicyFiles, PolicyFiles, CANDIDATE_PATHS};
use contrib_scout_core::store::{SharedStore, Store};
use contrib_scout_core::transport::fixture::Recorded;
use contrib_scout_core::transport::{ApiResponse, FixtureTransport};
use contrib_scout_core::{Freshness, RepoName, ScoutError};
use serde_json::{json, Map, Value};
use std::sync::Arc;

const ORG_SHA: &str = "1111111111111111111111111111111111111111";
const REPO_SHA: &str = "2222222222222222222222222222222222222222";
const REPO_SHA_2: &str = "3333333333333333333333333333333333333333";
const ORG_HEAD: &str = "/repos/acme/.github/commits/HEAD";
const REPO_HEAD: &str = "/repos/acme/widget/commits/HEAD";

struct Harness {
    fixture: Arc<FixtureTransport>,
    store: SharedStore,
    service: GitHubPolicyFiles,
}

fn harness(config: Config) -> Harness {
    let fixture = Arc::new(FixtureTransport::new());
    let store = Store::memory().unwrap().shared();
    let policy = HostPolicy {
        max_attempts: 1,
        ..HostPolicy::default()
    };
    let gateway = Gateway::new(fixture.clone(), policy, Budget::new(100))
        .with_cache(store.clone())
        .with_sleep(Arc::new(|_| {}));
    let service = GitHubPolicyFiles::new(Arc::new(gateway), store.clone(), config);
    Harness {
        fixture,
        store,
        service,
    }
}

fn repo() -> RepoName {
    "acme/widget".parse().unwrap()
}

fn commit(sha: &str, etag: &str) -> ApiResponse {
    ApiResponse::new(200, sha).with_etag(etag)
}

fn not_found() -> ApiResponse {
    ApiResponse::json(404, &json!({"message": "Not Found"}))
}

/// A tree entry for a blob.
fn blob_entry(name: &str, oid: &str, size: usize) -> Value {
    json!({"name": name, "type": "blob", "oid": oid, "object": {"byteSize": size}})
}

fn blob(oid: &str, text: &str) -> Value {
    json!({"oid": oid, "byteSize": text.len(), "isTruncated": false, "text": text})
}

/// One repository's part of the listing query: `dirs` per probe dir
/// (root, .github, docs), `texts` keyed by exact candidate path.
fn listing(dirs: [Option<Vec<Value>>; 3], texts: &[(&str, Value)]) -> Value {
    let mut repo = Map::new();
    for (k, dir) in dirs.into_iter().enumerate() {
        repo.insert(
            format!("d{k}"),
            dir.map_or(Value::Null, |entries| json!({ "entries": entries })),
        );
    }
    for (j, path) in CANDIDATE_PATHS.iter().enumerate() {
        let text = texts
            .iter()
            .find(|(p, _)| p == path)
            .map(|(_, v)| v.clone())
            .unwrap_or(Value::Null);
        repo.insert(format!("f{j}"), text);
    }
    Value::Object(repo)
}

fn graphql(data: Value) -> ApiResponse {
    ApiResponse::json(200, &json!({ "data": data }))
}

const ORG_CONTRIB: &str = "Org-wide: no AI-generated pull requests.\nSee AI_POLICY.\n";
const REPO_README: &str = "# Widget\nA widget.\n";
const REPO_CONTRIB: &str = "Line one\nLine two\nLine three\nLine four\n";
const REPO_DOCS: &str = "Docs contributing guide\n";

/// Cold fetch script: both HEADs, one listing, one blob query (for the
/// case-variant `docs/Contributing.md`).
fn script_cold(fixture: &FixtureTransport) {
    fixture.expect_get(ORG_HEAD, commit(ORG_SHA, "\"org-1\""));
    fixture.expect_get(REPO_HEAD, commit(REPO_SHA, "\"repo-1\""));
    let org = listing(
        [
            Some(vec![
                blob_entry("CONTRIBUTING.md", "o-contrib", ORG_CONTRIB.len()),
                json!({"name": "profile", "type": "tree", "oid": "o-profile", "object": {}}),
            ]),
            None,
            None,
        ],
        &[("CONTRIBUTING.md", blob("o-contrib", ORG_CONTRIB))],
    );
    let repo = listing(
        [
            Some(vec![
                blob_entry("README.md", "r-readme", REPO_README.len()),
                blob_entry("LICENSE", "r-license", 1000),
                json!({"name": "docs", "type": "tree", "oid": "r-docs", "object": {}}),
            ]),
            Some(vec![blob_entry(
                "CONTRIBUTING.md",
                "r-contrib",
                REPO_CONTRIB.len(),
            )]),
            Some(vec![blob_entry(
                "Contributing.md",
                "r-docs-contrib",
                REPO_DOCS.len(),
            )]),
        ],
        &[
            ("README.md", blob("r-readme", REPO_README)),
            (".github/CONTRIBUTING.md", blob("r-contrib", REPO_CONTRIB)),
        ],
    );
    fixture.expect_graphql(graphql(json!({"s0": org, "s1": repo})));
    fixture.expect_graphql(graphql(
        json!({"s0": {"b0": blob("r-docs-contrib", REPO_DOCS)}}),
    ));
}

#[test]
fn cold_fetch_lists_org_then_repo_in_four_requests() {
    let h = harness(Config::default());
    script_cold(&h.fixture);
    let fetch = h.service.fetch_report(&repo(), Freshness::Default).unwrap();
    h.fixture.assert_drained();
    assert_eq!(h.fixture.request_count(), 4);

    // The commit checks ask for the bare SHA.
    let requests = h.fixture.requests();
    let Recorded::Get(first) = &requests[0] else {
        panic!("expected a GET first")
    };
    assert_eq!(first.accept.as_deref(), Some(SHA_MEDIA_TYPE));
    // The listing is pinned to the commits just checked.
    let Recorded::Graphql { variables, .. } = &requests[2] else {
        panic!("expected the listing query")
    };
    assert_eq!(variables["d0_0"], json!(format!("{ORG_SHA}:")));
    assert_eq!(variables["d1_1"], json!(format!("{REPO_SHA}:.github")));

    let org: RepoName = "acme/.github".parse().unwrap();
    let summary: Vec<(String, String)> = fetch
        .files
        .iter()
        .map(|f| (f.repo.to_string(), f.path.clone()))
        .collect();
    assert_eq!(
        summary,
        vec![
            ("acme/.github".into(), "CONTRIBUTING.md".into()),
            ("acme/widget".into(), ".github/CONTRIBUTING.md".into()),
            ("acme/widget".into(), "docs/Contributing.md".into()),
            ("acme/widget".into(), "README.md".into()),
        ]
    );
    let org_file = &fetch.files[0];
    assert_eq!(org_file.url, pinned_url(&org, ORG_SHA, "CONTRIBUTING.md"));
    assert_eq!(
        org_file.url,
        format!("https://github.com/acme/.github/blob/{ORG_SHA}/CONTRIBUTING.md")
    );
    assert_eq!(org_file.text.as_deref(), Some(ORG_CONTRIB));
    assert_eq!(org_file.sha.as_deref(), Some("o-contrib"));
    assert!(!org_file.truncated);
    let docs = &fetch.files[2];
    assert_eq!(docs.text.as_deref(), Some(REPO_DOCS));
    assert_eq!(
        docs.url,
        format!("https://github.com/acme/widget/blob/{REPO_SHA}/docs/Contributing.md")
    );

    assert_eq!(fetch.sources.len(), 2);
    assert!(fetch
        .sources
        .iter()
        .all(|s| s.exists && s.origin == Origin::Network));
    assert!(fetch
        .absent
        .iter()
        .any(|a| a.repo == repo() && a.path == "AI_POLICY.md"));
    // The case-variant match is not reported absent.
    assert!(!fetch
        .absent
        .iter()
        .any(|a| a.repo == repo() && a.path.eq_ignore_ascii_case("docs/CONTRIBUTING.md")));
    assert_eq!(h.service.gateway.metrics().requests, 4);
}

#[test]
fn warm_fetch_within_ttl_makes_no_requests() {
    let h = harness(Config::default());
    script_cold(&h.fixture);
    let cold = h.service.fetch(&repo(), Freshness::Default).unwrap();
    let warm = h.service.fetch_report(&repo(), Freshness::Default).unwrap();
    assert_eq!(h.fixture.request_count(), 4, "no request within the TTL");
    assert!(warm.sources.iter().all(|s| s.origin == Origin::Cache));
    let strip = |files: &[contrib_scout_core::models::PolicyFile]| {
        files
            .iter()
            .map(|f| (f.path.clone(), f.url.clone(), f.text.clone()))
            .collect::<Vec<_>>()
    };
    assert_eq!(strip(&warm.files), strip(&cold));

    // Offline reads the same snapshot.
    let offline = h.service.fetch(&repo(), Freshness::Offline).unwrap();
    assert_eq!(strip(&offline), strip(&cold));
    assert_eq!(h.fixture.request_count(), 4);
}

#[test]
fn refresh_with_304s_serves_cached_text() {
    let h = harness(Config::default());
    script_cold(&h.fixture);
    let cold = h.service.fetch(&repo(), Freshness::Default).unwrap();

    h.fixture.expect_get(ORG_HEAD, ApiResponse::not_modified());
    h.fixture.expect_get(REPO_HEAD, ApiResponse::not_modified());
    let warm = h.service.fetch_report(&repo(), Freshness::Fresh).unwrap();
    h.fixture.assert_drained();
    assert_eq!(h.fixture.request_count(), 6, "two 304s, no GraphQL");
    let metrics = h.service.gateway.metrics();
    assert_eq!(metrics.served_304, 2);
    let requests = h.fixture.requests();
    let Recorded::Get(revalidate) = &requests[5] else {
        panic!("expected a GET")
    };
    assert_eq!(revalidate.if_none_match.as_deref(), Some("\"repo-1\""));
    assert!(warm.sources.iter().all(|s| s.origin == Origin::NotModified));
    assert_eq!(warm.files.len(), cold.len());
    for (a, b) in warm.files.iter().zip(&cold) {
        assert_eq!((&a.path, &a.url, &a.text), (&b.path, &b.url, &b.text));
    }
}

#[test]
fn moved_head_relists_only_that_source() {
    let h = harness(Config::default());
    script_cold(&h.fixture);
    h.service.fetch(&repo(), Freshness::Default).unwrap();

    h.fixture.expect_get(ORG_HEAD, ApiResponse::not_modified());
    h.fixture
        .expect_get(REPO_HEAD, commit(REPO_SHA_2, "\"repo-2\""));
    let relisted = listing(
        [
            Some(vec![blob_entry("AI_POLICY.md", "r-ai", 9)]),
            None,
            None,
        ],
        &[("AI_POLICY.md", blob("r-ai", "No LLMs.\n"))],
    );
    h.fixture.expect_graphql(graphql(json!({ "s0": relisted })));
    let fetch = h.service.fetch_report(&repo(), Freshness::Fresh).unwrap();
    h.fixture.assert_drained();
    let paths: Vec<&str> = fetch.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, vec!["CONTRIBUTING.md", "AI_POLICY.md"]);
    assert!(fetch.files[1].url.contains(REPO_SHA_2));
    // The old snapshot's files are gone from the cache.
    let store = h.store.lock().unwrap();
    assert_eq!(
        store
            .cached_policy_file(&repo(), "README.md")
            .unwrap()
            .unwrap()
            .data
            .status,
        404
    );
}

#[test]
fn absent_files_and_missing_org_repo_are_cached() {
    let h = harness(Config::default());
    h.fixture.expect_get(ORG_HEAD, not_found());
    h.fixture
        .expect_get(REPO_HEAD, commit(REPO_SHA, "\"repo-1\""));
    let repo_listing = listing(
        [
            Some(vec![blob_entry("README.md", "r-readme", 9)]),
            None,
            None,
        ],
        &[("README.md", blob("r-readme", "# Widget\n"))],
    );
    h.fixture
        .expect_graphql(graphql(json!({ "s0": repo_listing })));
    let fetch = h.service.fetch_report(&repo(), Freshness::Default).unwrap();
    h.fixture.assert_drained();
    assert_eq!(h.fixture.request_count(), 3);
    assert!(!fetch.sources[0].exists);
    assert_eq!(fetch.files.len(), 1);

    // Within the TTL nothing is refetched, neither the 404 nor the absent paths.
    let again = h.service.fetch_report(&repo(), Freshness::Default).unwrap();
    assert_eq!(h.fixture.request_count(), 3);
    assert_eq!(again.files.len(), 1);
    assert_eq!(again.absent.len(), fetch.absent.len());

    let store = h.store.lock().unwrap();
    let absent = store
        .cached_policy_file(&repo(), "ai_policy.md")
        .unwrap()
        .expect("absence is cached");
    assert_eq!(absent.data.status, 404);
    assert!(absent.data.file.is_none());
    assert_eq!(
        absent.data.url.as_deref(),
        Some(format!("https://github.com/acme/widget/blob/{REPO_SHA}/AI_POLICY.md").as_str())
    );
    let org: RepoName = "acme/.github".parse().unwrap();
    let marker = store.cached_policy_file(&org, "").unwrap().unwrap();
    assert_eq!(marker.data.status, 404);
}

#[test]
fn long_text_is_truncated_with_full_size() {
    let config = Config {
        policy_text_max_bytes: 10,
        ..Config::default()
    };
    let h = harness(config);
    let text = "0123456789abcdefghij-full-text";
    h.fixture.expect_get(ORG_HEAD, not_found());
    h.fixture
        .expect_get(REPO_HEAD, commit(REPO_SHA, "\"repo-1\""));
    let repo_listing = listing(
        [
            Some(vec![blob_entry("README.md", "r-readme", text.len())]),
            None,
            None,
        ],
        &[("README.md", blob("r-readme", text))],
    );
    h.fixture
        .expect_graphql(graphql(json!({ "s0": repo_listing })));
    let files = h.service.fetch(&repo(), Freshness::Default).unwrap();
    assert_eq!(files[0].text.as_deref(), Some("0123456789"));
    assert!(files[0].truncated);
    assert_eq!(files[0].size, text.len() as u64);
}

#[test]
fn graphql_errors_fail_the_fetch() {
    let h = harness(Config::default());
    h.fixture.expect_get(ORG_HEAD, not_found());
    h.fixture
        .expect_get(REPO_HEAD, commit(REPO_SHA, "\"repo-1\""));
    h.fixture.expect_graphql(ApiResponse::json(
        200,
        &json!({"data": null, "errors": [{"message": "Something went wrong"}]}),
    ));
    let error = h.service.fetch(&repo(), Freshness::Default).unwrap_err();
    assert!(
        format!("{error:#}").contains("Something went wrong"),
        "{error:#}"
    );
}

#[test]
fn live_fetch_revalidates_and_never_falls_back() {
    let h = harness(Config::default());
    script_cold(&h.fixture);
    h.service.fetch(&repo(), Freshness::Default).unwrap();

    // Live: both HEADs revalidated (304) even though the cache is young.
    h.fixture.expect_get(ORG_HEAD, ApiResponse::not_modified());
    h.fixture.expect_get(REPO_HEAD, ApiResponse::not_modified());
    let live = Freshness::Live.live().unwrap();
    let listed = h.service.fetch_live(&repo(), live, false).unwrap();
    assert!(listed.not_modified());
    assert_eq!(listed.get().len(), 4);
    assert!(listed.get().iter().all(|f| f.text.is_none()));

    h.fixture.expect_get(ORG_HEAD, ApiResponse::not_modified());
    h.fixture.expect_get(REPO_HEAD, ApiResponse::not_modified());
    let full = h.service.fetch_live(&repo(), live, true).unwrap();
    assert_eq!(full.get()[0].text.as_deref(), Some(ORG_CONTRIB));
    h.fixture.assert_drained();

    // Network down: an error, not the cached copy.
    h.fixture.expect_network_down(ORG_HEAD);
    let error = h.service.fetch_live(&repo(), live, true).unwrap_err();
    assert!(
        matches!(
            ScoutError::find(&error),
            Some(ScoutError::NetworkUnavailable { .. })
        ),
        "{error:#}"
    );
    // The default path, by contrast, still serves the young cache.
    assert_eq!(
        h.service.fetch(&repo(), Freshness::Default).unwrap().len(),
        4
    );
}

#[test]
fn offline_without_cache_is_not_found() {
    let h = harness(Config::default());
    let error = h.service.fetch(&repo(), Freshness::Offline).unwrap_err();
    assert!(matches!(
        ScoutError::find(&error),
        Some(ScoutError::NotFound { .. })
    ));
    assert_eq!(h.fixture.request_count(), 0);
}

#[test]
fn org_policy_repo_is_probed_once() {
    let h = harness(Config::default());
    let org: RepoName = "acme/.github".parse().unwrap();
    h.fixture.expect_get(ORG_HEAD, commit(ORG_SHA, "\"org-1\""));
    let org_listing = listing(
        [
            Some(vec![blob_entry("CONTRIBUTING.md", "o-contrib", 3)]),
            None,
            None,
        ],
        &[("CONTRIBUTING.md", blob("o-contrib", "Hi\n"))],
    );
    h.fixture
        .expect_graphql(graphql(json!({ "s0": org_listing })));
    let fetch = h.service.fetch_report(&org, Freshness::Default).unwrap();
    h.fixture.assert_drained();
    assert_eq!(fetch.sources.len(), 1);
    assert_eq!(fetch.files.len(), 1);
}
