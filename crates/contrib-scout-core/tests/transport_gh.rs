//! `GhTransport`: envelope parsing, exit-code handling, and argument building,
//! all with a canned process runner (no `gh` is spawned except where noted).

use contrib_scout_core::transport::gh::{
    parse_envelope, GhTransport, Invocation, ProcessOutput, ProcessRunner,
};
use contrib_scout_core::transport::{ApiRequest, Transport};
use contrib_scout_core::ScoutError;
use serde_json::{json, Value};
use std::io;
use std::sync::{Arc, Mutex};

/// Answers every run with one canned result and records invocations.
struct Canned {
    reply: Mutex<Option<io::Result<ProcessOutput>>>,
    seen: Mutex<Vec<Invocation>>,
}

impl Canned {
    fn output(exit_code: i32, stdout: &[u8], stderr: &str) -> Arc<Self> {
        Self::result(Ok(ProcessOutput {
            exit_code: Some(exit_code),
            stdout: stdout.to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }))
    }

    fn result(reply: io::Result<ProcessOutput>) -> Arc<Self> {
        Arc::new(Self {
            reply: Mutex::new(Some(reply)),
            seen: Mutex::new(Vec::new()),
        })
    }

    fn invocations(&self) -> Vec<Invocation> {
        self.seen.lock().unwrap().clone()
    }
}

impl ProcessRunner for Canned {
    fn run(&self, invocation: &Invocation) -> io::Result<ProcessOutput> {
        self.seen.lock().unwrap().push(invocation.clone());
        self.reply
            .lock()
            .unwrap()
            .take()
            .expect("runner called more than once")
    }
}

fn transport(runner: &Arc<Canned>) -> GhTransport {
    GhTransport::new("gh").with_runner(runner.clone())
}

fn code(error: &anyhow::Error) -> Option<&'static str> {
    ScoutError::find(error).map(ScoutError::code)
}

/// The exact shape gh 2.103 prints: status line ends in `\n`, headers in `\r\n`.
const REAL_200: &[u8] = b"HTTP/2.0 200 OK\n\
Content-Type: application/json; charset=utf-8\r\n\
Date: Tue, 25 Nov 2026 10:00:00 GMT\r\n\
Etag: W/\"abc123\"\r\n\
Link: <https://api.github.com/repositories/1/issues?page=2>; rel=\"next\", <https://api.github.com/repositories/1/issues?page=5>; rel=\"last\"\r\n\
Vary: Accept\r\n\
Vary: Accept-Encoding\r\n\
X-Ratelimit-Remaining: 4999\r\n\
\r\n\
[{\"number\":1}]";

#[test]
fn parses_real_gh_envelope_with_mixed_line_endings() {
    let response = parse_envelope(REAL_200).unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(response.body, b"[{\"number\":1}]");
    assert_eq!(response.etag(), Some("W/\"abc123\""));
    // Values with colons survive; names are lowercased; duplicates kept in order.
    assert_eq!(
        response.header_value("date"),
        Some("Tue, 25 Nov 2026 10:00:00 GMT")
    );
    assert_eq!(response.header_value("X-RateLimit-Remaining"), Some("4999"));
    let vary: Vec<_> = response
        .headers
        .iter()
        .filter(|(name, _)| name == "vary")
        .map(|(_, value)| value.as_str())
        .collect();
    assert_eq!(vary, ["Accept", "Accept-Encoding"]);
    assert!(response
        .headers
        .iter()
        .all(|(name, _)| *name == name.to_ascii_lowercase()));
    let link = response.header_value("link").unwrap();
    assert!(link.contains("rel=\"next\"") && link.contains("rel=\"last\""));
}

#[test]
fn parses_http11_crlf_and_http2_lf_status_lines() {
    let crlf = parse_envelope(b"HTTP/1.1 200 OK\r\nETag: \"x\"\r\n\r\n{}").unwrap();
    assert_eq!(
        (crlf.status, crlf.etag(), crlf.body.as_slice()),
        (200, Some("\"x\""), &b"{}"[..])
    );

    let lf = parse_envelope(b"HTTP/2 201\nlocation: /x\n\nbody\n").unwrap();
    assert_eq!(lf.status, 201);
    assert_eq!(lf.header_value("Location"), Some("/x"));
    // The body is byte-exact, including a trailing newline.
    assert_eq!(lf.body, b"body\n");

    let reason_with_spaces = parse_envelope(b"HTTP/2.0 304 Not Modified\n\r\n").unwrap();
    assert_eq!(reason_with_spaces.status, 304);
    assert!(reason_with_spaces.body.is_empty());
}

#[test]
fn rejects_malformed_envelopes() {
    for bad in [
        &b""[..],
        b"{\"message\":\"hi\"}",
        b"HTTP/2.0 2000 OK\n\r\n",
        b"HTTP/2.0 abc\n\r\n",
        b"HTTX/1.1 200 OK\r\n\r\n",
        // Headers never terminated: truncated output.
        b"HTTP/2.0 200 OK\nContent-Type: application/json\r\n",
        b"HTTP/2.0 200 OK\nnot-a-header\r\n\r\n{}",
    ] {
        assert!(
            parse_envelope(bad).is_none(),
            "{:?}",
            String::from_utf8_lossy(bad)
        );
    }
}

#[test]
fn not_modified_exit_1_is_a_response() {
    let runner = Canned::output(
        1,
        b"HTTP/2.0 304 Not Modified\nEtag: W/\"abc\"\r\nX-Ratelimit-Remaining: 4999\r\n\r\n",
        "gh: HTTP 304",
    );
    let response = transport(&runner)
        .get(&ApiRequest::get("/repos/o/r").etag(Some("W/\"abc\"".into())))
        .unwrap();
    assert!(response.is_not_modified());
    assert_eq!(response.etag(), Some("W/\"abc\""));
    assert!(response.body.is_empty());
}

#[test]
fn error_statuses_with_exit_1_are_responses() {
    for (status, stderr) in [
        (404, "gh: Not Found (HTTP 404)"),
        (403, "gh: HTTP 403"),
        (502, "gh: HTTP 502"),
    ] {
        let stdout = format!(
            "HTTP/2.0 {status} X\nContent-Type: application/json\r\n\r\n{{\"message\":\"m\"}}"
        );
        let runner = Canned::output(1, stdout.as_bytes(), stderr);
        let response = transport(&runner).get(&ApiRequest::get("/x")).unwrap();
        assert_eq!(response.status, status);
        assert_eq!(response.body, b"{\"message\":\"m\"}");
    }
}

#[test]
fn success_status_with_nonzero_exit_is_a_broken_transfer() {
    let runner = Canned::output(
        1,
        b"HTTP/2.0 200 OK\nEtag: \"x\"\r\n\r\n[{\"partial\"",
        "unexpected EOF",
    );
    let error = transport(&runner).get(&ApiRequest::get("/x")).unwrap_err();
    assert_eq!(code(&error), Some("network_unavailable"));
    assert!(error.to_string().contains("unexpected EOF"), "{error}");
}

#[test]
fn no_envelope_and_nonzero_exit_is_network_unavailable_with_stderr() {
    let runner = Canned::output(
        1,
        b"",
        "error connecting to api.github.com\ncheck your internet connection",
    );
    let error = transport(&runner).get(&ApiRequest::get("/x")).unwrap_err();
    assert_eq!(code(&error), Some("network_unavailable"));
    assert!(error.to_string().contains("error connecting"), "{error}");

    let auth = Canned::output(
        4,
        b"",
        "To get started with GitHub CLI, please run:  gh auth login",
    );
    let error = transport(&auth).get(&ApiRequest::get("/x")).unwrap_err();
    assert_eq!(code(&error), Some("network_unavailable"));
    assert!(error.to_string().contains("gh auth login"), "{error}");
}

#[test]
fn unparsable_output_with_exit_0_is_an_error() {
    let runner = Canned::output(0, b"{\"not\":\"an envelope\"}", "");
    let error = transport(&runner).get(&ApiRequest::get("/x")).unwrap_err();
    assert_eq!(code(&error), Some("network_unavailable"));
}

#[test]
fn spawn_failures_and_timeouts_are_network_unavailable() {
    let missing = Canned::result(Err(io::Error::new(io::ErrorKind::NotFound, "no such file")));
    let error = transport(&missing).get(&ApiRequest::get("/x")).unwrap_err();
    assert_eq!(code(&error), Some("network_unavailable"));
    assert!(error.to_string().contains("not found"), "{error}");

    let slow = Canned::result(Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "timed out after 30s",
    )));
    let error = transport(&slow).get(&ApiRequest::get("/x")).unwrap_err();
    assert_eq!(code(&error), Some("network_unavailable"));
    assert!(error.to_string().contains("timed out"), "{error}");
}

#[test]
fn missing_gh_executable_via_system_runner_is_network_unavailable() {
    let gh = GhTransport::new("Z:/definitely/not/here/gh-missing.exe");
    let error = gh.get(&ApiRequest::get("/rate_limit")).unwrap_err();
    assert_eq!(code(&error), Some("network_unavailable"));
}

#[test]
fn get_arguments_send_weak_etag_verbatim_and_force_get() {
    let runner = Canned::output(0, b"HTTP/2.0 200 OK\n\r\n[]", "");
    let mut request = ApiRequest::get("/repos/o/r/issues?state=open&per_page=100")
        .etag(Some("W/\"b3f6\"".into()))
        .accept("application/vnd.github.raw+json");
    request.headers.push(("X-Extra".into(), "1".into()));
    transport(&runner).get(&request).unwrap();

    let invocation = &runner.invocations()[0];
    assert_eq!(invocation.program.to_str(), Some("gh"));
    assert_eq!(invocation.stdin, None);
    assert_eq!(
        invocation.args,
        [
            "api",
            "-i",
            "--method",
            "GET",
            "--hostname",
            "github.com",
            "-H",
            "Accept: application/vnd.github.raw+json",
            "-H",
            "X-GitHub-Api-Version: 2022-11-28",
            "-H",
            "If-None-Match: W/\"b3f6\"",
            "-H",
            "X-Extra: 1",
            "repos/o/r/issues?state=open&per_page=100",
        ]
    );
    assert!(!invocation.args.iter().any(|arg| arg == "--cache"));
    assert!(invocation
        .env
        .contains(&("GH_PROMPT_DISABLED".into(), "1".into())));
    // The caller's GH_HOST / GH_REPO never reach gh.
    for name in ["GH_HOST", "GH_REPO"] {
        assert!(invocation.env_remove.iter().any(|n| n == name), "{name}");
        assert!(!invocation.env.iter().any(|(n, _)| n == name), "{name}");
    }
    let graphql = GhTransport::new("gh")
        .graphql_invocation("query { viewer { login } }", &serde_json::json!({}))
        .unwrap();
    assert_eq!(
        &graphql.args[..5],
        ["api", "graphql", "-i", "--hostname", "github.com"]
    );
    assert!(graphql.env_remove.iter().any(|n| n == "GH_HOST"));
}

#[test]
fn default_accept_and_absolute_link_urls() {
    let gh = GhTransport::new("gh");
    let invocation = gh
        .get_invocation(&ApiRequest::get(
            "https://api.github.com/repositories/1/issues?page=2",
        ))
        .unwrap();
    assert!(invocation
        .args
        .contains(&"Accept: application/vnd.github+json".to_owned()));
    assert_eq!(
        invocation.args.last().unwrap(),
        "repositories/1/issues?page=2"
    );
    assert!(!invocation
        .args
        .iter()
        .any(|arg| arg.starts_with("If-None-Match")));

    // A query that merely mentions a URL is still a relative path.
    let search = gh
        .get_invocation(&ApiRequest::get("/search/issues?q=https://x"))
        .unwrap();
    assert_eq!(search.args.last().unwrap(), "search/issues?q=https://x");
}

#[test]
fn refuses_foreign_origins_and_unsafe_paths_without_spawning() {
    for path in [
        "https://evil.example/repos/o/r",
        "https://api.github.com.evil.example/x",
        "https://api.github.com@evil.example/x",
        "http://api.github.com/x",
        "/repos/{owner}/{repo}",
        "/x#frag",
        "/",
        "/x\r\nInjected: 1",
    ] {
        let runner = Canned::output(0, b"HTTP/2.0 200 OK\n\r\n", "");
        assert!(
            transport(&runner).get(&ApiRequest::get(path)).is_err(),
            "{path}"
        );
        assert!(runner.invocations().is_empty(), "{path} spawned gh");
    }
    let mut bad_header = ApiRequest::get("/x");
    bad_header
        .headers
        .push(("X-A".into(), "v\r\nInjected: 1".into()));
    assert!(GhTransport::new("gh").get_invocation(&bad_header).is_err());
}

#[test]
fn enterprise_hostname_is_passed_and_origin_matched() {
    let gh = GhTransport::new("gh").with_hostname("ghe.example.com");
    let invocation = gh
        .get_invocation(&ApiRequest::get(
            "https://ghe.example.com/api/v3/repos/o/r?page=2",
        ))
        .unwrap();
    assert_eq!(
        &invocation.args[..6],
        [
            "api",
            "-i",
            "--method",
            "GET",
            "--hostname",
            "ghe.example.com"
        ]
    );
    assert_eq!(invocation.args.last().unwrap(), "repos/o/r?page=2");
    assert!(gh
        .get_invocation(&ApiRequest::get("https://api.github.com/x"))
        .is_err());
}

#[test]
fn graphql_sends_json_body_on_stdin() {
    let runner = Canned::output(
        0,
        b"HTTP/2.0 200 OK\nContent-Type: application/json\r\n\r\n{\"data\":{\"viewer\":{\"login\":\"x\"}}}",
        "",
    );
    let variables = json!({ "n": 1, "owner": "o" });
    let response = transport(&runner)
        .graphql("query($n:Int!){ viewer { login } }", &variables)
        .unwrap();
    assert_eq!(response.status, 200);

    let invocation = &runner.invocations()[0];
    assert_eq!(
        invocation.args,
        [
            "api",
            "graphql",
            "-i",
            "--hostname",
            "github.com",
            "-H",
            "X-GitHub-Api-Version: 2022-11-28",
            "--input",
            "-"
        ]
    );
    let body: Value = serde_json::from_slice(invocation.stdin.as_ref().unwrap()).unwrap();
    assert_eq!(body["query"], "query($n:Int!){ viewer { login } }");
    assert_eq!(body["variables"], variables);
}

#[test]
fn graphql_errors_with_exit_1_are_returned_as_responses() {
    let runner = Canned::output(
        1,
        b"HTTP/2.0 200 OK\nContent-Type: application/json\r\n\r\n{\"errors\":[{\"message\":\"Field 'nope' doesn't exist\"}]}",
        "gh: Field 'nope' doesn't exist on type 'User'",
    );
    let response = transport(&runner)
        .graphql("query { viewer { nope } }", &json!({}))
        .unwrap();
    assert_eq!(response.status, 200);
    let body: Value = response.json_body().unwrap();
    assert!(body["errors"].is_array());

    // Without an `errors` body a 200 + exit 1 is still a broken transfer.
    let truncated = Canned::output(1, b"HTTP/2.0 200 OK\n\r\n{\"data\":", "unexpected EOF");
    let error = transport(&truncated)
        .graphql("query { viewer { login } }", &json!({}))
        .unwrap_err();
    assert_eq!(code(&error), Some("network_unavailable"));
}

/// Live: needs `gh auth status` to succeed. `cargo test --test transport_gh -- --ignored`
#[test]
#[ignore = "calls the real gh and GitHub"]
fn live_gh_rate_limit() {
    let gh = GhTransport::new("gh");
    let response = gh.get(&ApiRequest::get("/rate_limit")).unwrap();
    assert_eq!(response.status, 200);
    assert!(response.header_value("x-ratelimit-limit").is_some());
    let body: Value = response.json_body().unwrap();
    assert!(body["resources"]["core"]["limit"].is_u64());

    // Conditional request on a cacheable resource: expect 304 (or 200 if it changed).
    let repo = gh.get(&ApiRequest::get("/repos/cli/cli")).unwrap();
    assert_eq!(repo.status, 200);
    let etag = repo.etag().expect("repo has an ETag").to_owned();
    let again = gh
        .get(&ApiRequest::get("/repos/cli/cli").etag(Some(etag)))
        .unwrap();
    assert!(matches!(again.status, 200 | 304), "{}", again.status);

    let graphql = gh
        .graphql("query { rateLimit { limit } }", &json!({}))
        .unwrap();
    assert_eq!(graphql.status, 200);
}
