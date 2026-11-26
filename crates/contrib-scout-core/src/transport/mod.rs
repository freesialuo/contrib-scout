//! The only outbound channel to GitHub (§3, §4).
//!
//! A transport sends exactly one attempt: no retries, no budget, no caching —
//! that is [`crate::gateway::Gateway`]'s job. Contract:
//! - Any HTTP status, including 304 and 4xx/5xx, is `Ok(ApiResponse)`.
//! - `Err` means no HTTP response was obtained; implementations should use
//!   [`crate::ScoutError::NetworkUnavailable`] so gates fail closed.
//! - ETags (including weak `W/"…"`) are sent and returned verbatim (§2).

pub mod fixture;
pub mod gh;
pub mod http;

pub use fixture::FixtureTransport;
pub use gh::GhTransport;
pub use http::HttpTransport;

use crate::coordinator::Resource;
use crate::error::Result;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// `Accept` sent when [`ApiRequest::accept`] is `None`.
pub const DEFAULT_ACCEPT: &str = "application/vnd.github+json";
/// `X-GitHub-Api-Version` sent by the real transports.
pub const API_VERSION: &str = "2022-11-28";

/// A REST GET. `path` is relative to `https://api.github.com` (e.g.
/// `/repos/o/r/issues?state=open`) or an absolute same-origin URL taken from a
/// `Link` header; the gateway verifies the origin before sending.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiRequest {
    pub path: String,
    /// Rate-limit bucket this request counts against.
    pub resource: Resource,
    /// Sent as `If-None-Match` verbatim.
    pub if_none_match: Option<String>,
    /// `Accept` header; `None` means `application/vnd.github+json`.
    pub accept: Option<String>,
    /// Extra headers (name, value).
    pub headers: Vec<(String, String)>,
}

impl ApiRequest {
    pub fn get(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            resource: Resource::Core,
            if_none_match: None,
            accept: None,
            headers: Vec::new(),
        }
    }

    pub fn resource(mut self, resource: Resource) -> Self {
        self.resource = resource;
        self
    }

    pub fn etag(mut self, etag: Option<String>) -> Self {
        self.if_none_match = etag;
        self
    }

    pub fn accept(mut self, accept: impl Into<String>) -> Self {
        self.accept = Some(accept.into());
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiResponse {
    pub status: u16,
    /// Header names are lowercased; order and duplicates are preserved.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl ApiResponse {
    pub fn new(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: body.into(),
        }
    }

    pub fn json(status: u16, body: &Value) -> Self {
        Self::new(status, body.to_string()).header("content-type", "application/json")
    }

    pub fn not_modified() -> Self {
        Self::new(304, Vec::new())
    }

    /// Adds a header (name lowercased).
    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers
            .push((name.to_ascii_lowercase(), value.to_owned()));
        self
    }

    pub fn with_etag(self, etag: &str) -> Self {
        self.header("etag", etag)
    }

    /// First header value with this name, case-insensitively.
    pub fn header_value(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub fn etag(&self) -> Option<&str> {
        self.header_value("etag")
    }

    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub fn is_not_modified(&self) -> bool {
        self.status == 304
    }

    pub fn text(&self) -> Result<&str> {
        Ok(std::str::from_utf8(&self.body)?)
    }

    pub fn json_body<T: DeserializeOwned>(&self) -> Result<T> {
        Ok(serde_json::from_slice(&self.body)?)
    }
}

pub trait Transport: Send + Sync {
    /// Short name for diagnostics (`gh`, `http`, `fixture`).
    fn name(&self) -> &'static str;

    /// One REST GET attempt.
    fn get(&self, request: &ApiRequest) -> Result<ApiResponse>;

    /// One GraphQL POST to `/graphql` (counts against [`Resource::Graphql`]).
    fn graphql(&self, query: &str, variables: &Value) -> Result<ApiResponse>;
}
