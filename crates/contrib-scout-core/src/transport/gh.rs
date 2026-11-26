//! Default transport: `gh api -i` subprocess (§2, §11.1).
//!
//! Verified behaviour of `gh api -i` (gh 2.103, §2):
//! - stdout is `<status line>\n<headers>\r\n\r\n<raw body>`. The status line
//!   ends in `\n` while headers end in `\r\n`; both styles are accepted.
//! - `gh` exits 1 on 304 (stderr `gh: HTTP 304`) and on every 4xx/5xx, still
//!   printing the envelope. The parsed status decides, never the exit code.
//! - `gh api graphql` exits 1 on HTTP 200 when the body carries `errors`; that
//!   response is returned as-is so the caller can read the errors.
//! - A non-zero exit with any other 2xx status is a broken transfer and an error.
//! - No envelope at all means no HTTP response: `ScoutError::NetworkUnavailable`
//!   with gh's stderr.
//!
//! Requests are passed as an argument vector (no shell). GraphQL sends
//! `{"query", "variables"}` as JSON on stdin (`--input -`), which keeps variable
//! types exact and avoids Windows command-line length limits. Weak ETags
//! (`W/"…"`) are sent back verbatim. `gh api --cache` is never used.
//! Absolute URLs (from `Link` headers) must be on the API origin and are passed
//! to gh as relative paths.

use super::{ApiRequest, ApiResponse, Transport, API_VERSION, DEFAULT_ACCEPT};
use crate::error::{Result, ScoutError};
use anyhow::{bail, Context};
use serde_json::{json, Value};
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// One subprocess invocation, fully described so tests can inspect it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invocation {
    pub program: PathBuf,
    pub args: Vec<String>,
    /// Environment variables set for the child (added to the inherited ones).
    pub env: Vec<(String, String)>,
    /// Environment variables removed from the child.
    pub env_remove: Vec<String>,
    /// Bytes written to the child's stdin; `None` means stdin is closed.
    pub stdin: Option<Vec<u8>>,
    pub timeout: Duration,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProcessOutput {
    /// `None` when the process was killed by a signal.
    pub exit_code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl ProcessOutput {
    pub fn success(&self) -> bool {
        self.exit_code == Some(0)
    }
}

/// Runs a subprocess. Injected into [`GhTransport`] so tests can supply
/// canned output without spawning `gh`.
pub trait ProcessRunner: Send + Sync {
    /// `Err` means the process could not be started or did not finish in time
    /// (`io::ErrorKind::TimedOut`).
    fn run(&self, invocation: &Invocation) -> io::Result<ProcessOutput>;
}

/// Spawns real processes via [`std::process::Command`].
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemRunner;

impl ProcessRunner for SystemRunner {
    fn run(&self, invocation: &Invocation) -> io::Result<ProcessOutput> {
        let mut command = Command::new(&invocation.program);
        command
            .args(&invocation.args)
            .stdin(if invocation.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for key in &invocation.env_remove {
            command.env_remove(key);
        }
        for (key, value) in &invocation.env {
            command.env(key, value);
        }
        let mut child = command.spawn()?;

        let writer = match (child.stdin.take(), invocation.stdin.clone()) {
            (Some(mut pipe), Some(bytes)) => Some(thread::spawn(move || {
                // A child that exits early closes the pipe; its exit status reports why.
                let _ = pipe.write_all(&bytes);
            })),
            _ => None,
        };
        let stdout = read_in_background(child.stdout.take());
        let stderr = read_in_background(child.stderr.take());

        let deadline = Instant::now() + invocation.timeout;
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("timed out after {}s", invocation.timeout.as_secs_f64()),
                ));
            }
            thread::sleep(Duration::from_millis(10));
        };
        if let Some(writer) = writer {
            let _ = writer.join();
        }
        Ok(ProcessOutput {
            exit_code: status.code(),
            stdout: join_reader(stdout)?,
            stderr: join_reader(stderr)?,
        })
    }
}

type Reader = Option<thread::JoinHandle<io::Result<Vec<u8>>>>;

fn read_in_background<R: Read + Send + 'static>(pipe: Option<R>) -> Reader {
    pipe.map(|mut pipe| {
        thread::spawn(move || {
            let mut buffer = Vec::new();
            pipe.read_to_end(&mut buffer)?;
            Ok(buffer)
        })
    })
}

fn join_reader(reader: Reader) -> io::Result<Vec<u8>> {
    match reader {
        Some(handle) => handle
            .join()
            .map_err(|_| io::Error::other("pipe reader panicked"))?,
        None => Ok(Vec::new()),
    }
}

pub struct GhTransport {
    /// The `gh` executable (config `gh_path`).
    pub gh: PathBuf,
    /// GitHub host, `github.com` unless GHES.
    pub hostname: String,
    /// Upper bound for one `gh` invocation.
    pub timeout: Duration,
    runner: Arc<dyn ProcessRunner>,
}

impl GhTransport {
    pub fn new(gh: impl Into<PathBuf>) -> Self {
        Self {
            gh: gh.into(),
            hostname: "github.com".into(),
            timeout: Duration::from_secs(30),
            runner: Arc::new(SystemRunner),
        }
    }

    /// Replaces the process runner (tests).
    pub fn with_runner(mut self, runner: Arc<dyn ProcessRunner>) -> Self {
        self.runner = runner;
        self
    }

    pub fn with_hostname(mut self, hostname: impl Into<String>) -> Self {
        self.hostname = hostname.into();
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// REST API origin for [`Self::hostname`].
    pub fn api_origin(&self) -> String {
        if self.hostname.eq_ignore_ascii_case("github.com") {
            "https://api.github.com".into()
        } else {
            format!("https://{}/api/v3", self.hostname)
        }
    }

    /// The `gh` invocation for a REST GET (no process is started).
    pub fn get_invocation(&self, request: &ApiRequest) -> Result<Invocation> {
        let endpoint = self.endpoint(&request.path)?;
        let mut args = self.base_args(["api", "-i", "--method", "GET"]);
        let accept = request.accept.as_deref().unwrap_or(DEFAULT_ACCEPT);
        push_header(&mut args, "Accept", accept)?;
        push_header(&mut args, "X-GitHub-Api-Version", API_VERSION)?;
        if let Some(etag) = &request.if_none_match {
            push_header(&mut args, "If-None-Match", etag)?;
        }
        for (name, value) in &request.headers {
            push_header(&mut args, name, value)?;
        }
        args.push(endpoint);
        Ok(self.invocation(args, None))
    }

    /// The `gh` invocation for a GraphQL query (no process is started).
    pub fn graphql_invocation(&self, query: &str, variables: &Value) -> Result<Invocation> {
        let mut args = self.base_args(["api", "graphql", "-i"]);
        push_header(&mut args, "X-GitHub-Api-Version", API_VERSION)?;
        args.extend(["--input".to_owned(), "-".to_owned()]);
        let body = json!({ "query": query, "variables": variables });
        Ok(self.invocation(args, Some(serde_json::to_vec(&body)?)))
    }

    /// Always names the host: with `GH_HOST` removed from the child's
    /// environment, the target never depends on the caller's shell.
    fn base_args<const N: usize>(&self, leading: [&str; N]) -> Vec<String> {
        let mut args: Vec<String> = leading.iter().map(|&arg| arg.to_owned()).collect();
        args.extend(["--hostname".to_owned(), self.hostname.clone()]);
        args
    }

    fn invocation(&self, args: Vec<String>, stdin: Option<Vec<u8>>) -> Invocation {
        Invocation {
            program: self.gh.clone(),
            args,
            env: [
                ("GH_PROMPT_DISABLED", "1"),
                ("GH_NO_UPDATE_NOTIFIER", "1"),
                ("NO_COLOR", "1"),
            ]
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect(),
            // `GH_HOST` / `GH_REPO` would redirect requests to another host or
            // fill `{owner}/{repo}`; the host is always passed as `--hostname`.
            env_remove: vec![
                "GH_FORCE_TTY".into(),
                "CLICOLOR_FORCE".into(),
                "GH_HOST".into(),
                "GH_REPO".into(),
            ],
            stdin,
            timeout: self.timeout,
        }
    }

    /// The endpoint argument for gh: a path relative to the API root.
    fn endpoint(&self, path: &str) -> Result<String> {
        let absolute = strip_prefix_ignore_case(path, "https://").is_some()
            || strip_prefix_ignore_case(path, "http://").is_some();
        let relative = if absolute {
            let origin = self.api_origin();
            let rest = strip_prefix_ignore_case(path, &origin)
                .filter(|rest| rest.starts_with('/'))
                .with_context(|| format!("refusing to send a request outside {origin}"))?;
            rest.to_owned()
        } else {
            path.to_owned()
        };
        let relative = relative.trim_start_matches('/');
        if relative.is_empty() {
            bail!("empty API path");
        }
        if relative.contains('#') {
            bail!("API path must not contain a fragment: {path}");
        }
        // gh substitutes {owner}/{repo}/{branch} from the current git checkout.
        if relative.contains(['{', '}']) {
            bail!("API path must not contain braces: {path}");
        }
        if relative.chars().any(char::is_control) {
            bail!("API path must not contain control characters");
        }
        Ok(relative.to_owned())
    }

    fn execute(&self, invocation: &Invocation, graphql: bool) -> Result<ApiResponse> {
        let what = if graphql {
            "gh api graphql".to_owned()
        } else {
            format!(
                "gh api {}",
                invocation.args.last().map_or("", String::as_str)
            )
        };
        let output = self.runner.run(invocation).map_err(|error| {
            let message = match error.kind() {
                io::ErrorKind::NotFound => {
                    format!("gh executable not found: {}", self.gh.display())
                }
                io::ErrorKind::TimedOut => format!("{what}: {error}"),
                _ => format!("failed to run {}: {error}", self.gh.display()),
            };
            anyhow::Error::from(ScoutError::NetworkUnavailable { message })
        })?;
        interpret(&what, &output, graphql)
    }
}

/// Decides the outcome of one finished `gh api -i` run.
fn interpret(what: &str, output: &ProcessOutput, graphql: bool) -> Result<ApiResponse> {
    let stderr = stderr_excerpt(&output.stderr);
    let Some(response) = parse_envelope(&output.stdout) else {
        let message = if output.success() {
            format!("{what}: unparsable output (no HTTP status line)")
        } else {
            format!(
                "{what} failed (exit {}): {stderr}",
                exit_label(output.exit_code)
            )
        };
        return Err(ScoutError::NetworkUnavailable { message }.into());
    };
    if output.success() || !response.is_success() {
        return Ok(response);
    }
    if graphql && response.status == 200 && has_graphql_errors(&response.body) {
        return Ok(response);
    }
    Err(ScoutError::NetworkUnavailable {
        message: format!(
            "{what}: HTTP {} but gh exited {} (incomplete transfer): {stderr}",
            response.status,
            exit_label(output.exit_code)
        ),
    }
    .into())
}

fn has_graphql_errors(body: &[u8]) -> bool {
    serde_json::from_slice::<Value>(body)
        .ok()
        .is_some_and(|value| value.get("errors").is_some())
}

fn exit_label(code: Option<i32>) -> String {
    code.map_or_else(|| "by signal".to_owned(), |code| code.to_string())
}

fn stderr_excerpt(stderr: &[u8]) -> String {
    const LIMIT: usize = 500;
    let text = String::from_utf8_lossy(stderr);
    let text = text.trim();
    if text.is_empty() {
        return "(no stderr)".into();
    }
    match text.char_indices().nth(LIMIT) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_owned(),
    }
}

/// Parses `gh api -i` stdout into status, headers and body.
///
/// Returns `None` unless the output starts with an `HTTP/<version> <3-digit>`
/// status line and contains the blank line that ends the headers. Lines may end
/// in `\n` or `\r\n` (gh mixes both). Header names are lowercased; order and
/// duplicates are kept; the body is returned byte-for-byte.
pub fn parse_envelope(stdout: &[u8]) -> Option<ApiResponse> {
    let mut rest = stdout;
    let status_line = next_line(&mut rest)?;
    let status = parse_status_line(std::str::from_utf8(status_line).ok()?)?;
    let mut headers = Vec::new();
    loop {
        let line = next_line(&mut rest)?;
        if line.is_empty() {
            break;
        }
        let line = String::from_utf8_lossy(line);
        let (name, value) = line.split_once(':')?;
        let name = name.trim();
        if name.is_empty() {
            return None;
        }
        headers.push((name.to_ascii_lowercase(), value.trim().to_owned()));
    }
    Some(ApiResponse {
        status,
        headers,
        body: rest.to_vec(),
    })
}

/// Splits off one line (without `\n` / `\r\n`); `None` if no newline remains.
fn next_line<'a>(rest: &mut &'a [u8]) -> Option<&'a [u8]> {
    let end = rest.iter().position(|&byte| byte == b'\n')?;
    let line = &rest[..end];
    *rest = &rest[end + 1..];
    Some(line.strip_suffix(b"\r").unwrap_or(line))
}

fn parse_status_line(line: &str) -> Option<u16> {
    let mut parts = line.split_whitespace();
    let version = parts.next()?;
    if !version.starts_with("HTTP/") {
        return None;
    }
    let code = parts.next()?;
    if code.len() != 3 || !code.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    code.parse()
        .ok()
        .filter(|status| (100..600).contains(status))
}

fn push_header(args: &mut Vec<String>, name: &str, value: &str) -> Result<()> {
    if name.is_empty() || name.contains([':', '\r', '\n']) || value.contains(['\r', '\n']) {
        bail!("invalid header {name:?}");
    }
    args.push("-H".into());
    args.push(format!("{name}: {value}"));
    Ok(())
}

fn strip_prefix_ignore_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &text[prefix.len()..])
}

impl Transport for GhTransport {
    fn name(&self) -> &'static str {
        "gh"
    }

    fn get(&self, request: &ApiRequest) -> Result<ApiResponse> {
        let invocation = self.get_invocation(request)?;
        self.execute(&invocation, false)
    }

    fn graphql(&self, query: &str, variables: &Value) -> Result<ApiResponse> {
        let invocation = self.graphql_invocation(query, variables)?;
        self.execute(&invocation, true)
    }
}
