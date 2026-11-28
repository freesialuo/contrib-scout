//! Alternative transport: blocking reqwest with a token from `gh auth token` (§11.1).
//! Faster for bulk sync. Same contract as [`super::Transport`]: no redirects,
//! no internal retries, any HTTP status (304 included) is `Ok`.
//!
//! Only URLs on the [`HttpTransport::api_base`] origin are sent; the base must
//! be `https` (plain `http` is accepted only for a loopback host, for tests).
//! The token is taken from the explicit field, else `GH_TOKEN` / `GITHUB_TOKEN`
//! (gh's precedence), else `gh auth token`, and is never sent to any other
//! origin. Without a token no request is sent: anonymous requests would get
//! 60/h and then fail as a misleading quota error.

use super::{ApiRequest, ApiResponse, Transport, API_VERSION, DEFAULT_ACCEPT};
use crate::error::{Result, ScoutError};
use anyhow::{anyhow, bail, Context};
use reqwest::blocking::{Client, RequestBuilder};
use reqwest::header::{ACCEPT, CONTENT_TYPE, IF_NONE_MATCH};
use reqwest::redirect::Policy;
use reqwest::Url;
use serde_json::{json, Value};
use std::env;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::Duration;

/// Responses larger than this are rejected rather than buffered.
pub const MAX_BODY_BYTES: u64 = 64 * 1024 * 1024;

const USER_AGENT: &str = concat!("contrib-scout/", env!("CARGO_PKG_VERSION"));

pub struct HttpTransport {
    /// `https://api.github.com` unless GHES.
    pub api_base: String,
    /// Explicit token; when `None` it is looked up lazily via
    /// `GH_TOKEN`/`GITHUB_TOKEN`, then `gh auth token`.
    pub token: Option<String>,
    pub timeout: Duration,
    /// `gh` executable used for `gh auth token`.
    pub gh: PathBuf,
    client: OnceLock<Client>,
    looked_up_token: OnceLock<Option<String>>,
}

impl HttpTransport {
    pub fn new(token: Option<String>) -> Self {
        Self {
            api_base: "https://api.github.com".into(),
            token,
            timeout: Duration::from_secs(15),
            gh: "gh".into(),
            client: OnceLock::new(),
            looked_up_token: OnceLock::new(),
        }
    }

    pub fn with_api_base(mut self, api_base: impl Into<String>) -> Self {
        self.api_base = api_base.into().trim_end_matches('/').to_owned();
        self
    }

    pub fn with_gh(mut self, gh: impl Into<PathBuf>) -> Self {
        self.gh = gh.into();
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Resolves a request path (relative, or absolute from a `Link` header)
    /// to a URL on the API origin, refusing anything else.
    pub fn request_url(&self, path: &str) -> Result<Url> {
        let base = Url::parse(&self.api_base)
            .with_context(|| format!("invalid api_base {:?}", self.api_base))?;
        let loopback = matches!(base.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
        if !(base.scheme() == "https" || base.scheme() == "http" && loopback) {
            bail!("api_base must use https: {}", self.api_base);
        }
        let lower = path.get(..8).unwrap_or(path).to_ascii_lowercase();
        let url = if lower.starts_with("https://") || lower.starts_with("http://") {
            Url::parse(path).with_context(|| format!("invalid API URL {path:?}"))?
        } else {
            let base = self.api_base.trim_end_matches('/');
            let path = path.trim_start_matches('/');
            Url::parse(&format!("{base}/{path}"))
                .with_context(|| format!("invalid API path {path:?}"))?
        };
        let base_path = base.path().trim_end_matches('/');
        let same_origin = url.scheme() == base.scheme()
            && url.host_str() == base.host_str()
            && url.port_or_known_default() == base.port_or_known_default()
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none()
            && (base_path.is_empty()
                || url.path() == base_path
                || url.path().starts_with(&format!("{base_path}/")));
        if !same_origin {
            bail!("refusing to send a request outside {}", self.api_base);
        }
        Ok(url)
    }

    fn client(&self) -> Result<&Client> {
        if let Some(client) = self.client.get() {
            return Ok(client);
        }
        let client = Client::builder()
            .redirect(Policy::none())
            .user_agent(USER_AGENT)
            .build()
            .context("failed to build HTTP client")?;
        Ok(self.client.get_or_init(|| client))
    }

    fn token(&self) -> Result<&str> {
        if let Some(token) = &self.token {
            return Ok(token);
        }
        self.looked_up_token
            .get_or_init(|| {
                github_token(&self.gh, &self.token_hostname(), |key| env::var(key).ok())
            })
            .as_deref()
            .ok_or_else(|| {
                anyhow!("no GitHub token: set GH_TOKEN/GITHUB_TOKEN or run gh auth login")
            })
    }

    fn token_hostname(&self) -> String {
        match Url::parse(&self.api_base)
            .ok()
            .and_then(|url| url.host_str().map(str::to_owned))
        {
            Some(host) if host.eq_ignore_ascii_case("api.github.com") => "github.com".into(),
            Some(host) => host,
            None => "github.com".into(),
        }
    }

    fn send(&self, builder: RequestBuilder, what: &str) -> Result<ApiResponse> {
        let builder = builder
            .timeout(self.timeout)
            .header("X-GitHub-Api-Version", API_VERSION)
            .bearer_auth(self.token()?);
        let response = builder.send().map_err(|error| network(what, &error))?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_ascii_lowercase(),
                    String::from_utf8_lossy(value.as_bytes()).into_owned(),
                )
            })
            .collect();
        let mut body = Vec::new();
        response
            .take(MAX_BODY_BYTES + 1)
            .read_to_end(&mut body)
            .map_err(|error| ScoutError::NetworkUnavailable {
                message: format!("{what}: reading body failed: {error}"),
            })?;
        if body.len() as u64 > MAX_BODY_BYTES {
            bail!("{what}: response exceeds {MAX_BODY_BYTES} bytes");
        }
        Ok(ApiResponse {
            status,
            headers,
            body,
        })
    }
}

fn network(what: &str, error: &reqwest::Error) -> anyhow::Error {
    let mut message = format!("{what}: {error}");
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        message.push_str(&format!(": {cause}"));
        source = cause.source();
    }
    ScoutError::NetworkUnavailable { message }.into()
}

/// `GH_TOKEN`, then `GITHUB_TOKEN` (`var` reads the environment), then
/// `gh auth token --hostname <host>`.
fn github_token(
    gh: &PathBuf,
    hostname: &str,
    var: impl Fn(&str) -> Option<String>,
) -> Option<String> {
    for key in ["GH_TOKEN", "GITHUB_TOKEN"] {
        if let Some(token) = var(key).and_then(nonempty) {
            return Some(token);
        }
    }
    let output = Command::new(gh)
        .args(["auth", "token", "--hostname", hostname])
        .env("GH_PROMPT_DISABLED", "1")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if output.status.success() {
        String::from_utf8(output.stdout).ok().and_then(nonempty)
    } else {
        None
    }
}

fn nonempty(value: String) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

impl Transport for HttpTransport {
    fn name(&self) -> &'static str {
        "http"
    }

    fn get(&self, request: &ApiRequest) -> Result<ApiResponse> {
        let url = self.request_url(&request.path)?;
        let what = format!("GET {}", url.path());
        let mut builder = self
            .client()?
            .get(url)
            .header(ACCEPT, request.accept.as_deref().unwrap_or(DEFAULT_ACCEPT));
        if let Some(etag) = &request.if_none_match {
            builder = builder.header(IF_NONE_MATCH, etag);
        }
        for (name, value) in &request.headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        self.send(builder, &what)
    }

    fn graphql(&self, query: &str, variables: &Value) -> Result<ApiResponse> {
        let url = self.request_url("/graphql")?;
        let body = json!({ "query": query, "variables": variables });
        let builder = self
            .client()?
            .post(url)
            .header(ACCEPT, "application/json")
            .header(CONTENT_TYPE, "application/json")
            .body(serde_json::to_vec(&body)?);
        self.send(builder, "POST /graphql")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NO_GH: &str = "contrib-scout-test-missing-gh";

    #[test]
    fn gh_token_takes_precedence_over_github_token() {
        let both = |key: &str| Some(format!("{key}-value"));
        assert_eq!(
            github_token(&NO_GH.into(), "github.com", both).as_deref(),
            Some("GH_TOKEN-value")
        );
        let only_github = |key: &str| (key == "GITHUB_TOKEN").then(|| "gt".to_owned());
        assert_eq!(
            github_token(&NO_GH.into(), "github.com", only_github).as_deref(),
            Some("gt")
        );
        let blank_gh = |key: &str| Some(if key == "GH_TOKEN" { " " } else { "gt" }.to_owned());
        assert_eq!(
            github_token(&NO_GH.into(), "github.com", blank_gh).as_deref(),
            Some("gt")
        );
    }

    #[test]
    fn no_env_token_and_no_gh_finds_nothing() {
        assert_eq!(github_token(&NO_GH.into(), "github.com", |_| None), None);
    }
}
