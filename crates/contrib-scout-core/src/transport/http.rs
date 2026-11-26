//! Alternative transport: blocking reqwest with a token from `gh auth token` (§11.1).
//! Faster for bulk sync. Same contract as [`super::Transport`]: no redirects,
//! no internal retries, any HTTP status is `Ok`.

use super::{ApiRequest, ApiResponse, Transport};
use crate::error::{not_implemented, Result};
use serde_json::Value;
use std::time::Duration;

pub struct HttpTransport {
    /// `https://api.github.com` unless GHES.
    pub api_base: String,
    /// Obtained lazily via `gh auth token` (or `GH_TOKEN`/`GITHUB_TOKEN`).
    pub token: Option<String>,
    pub timeout: Duration,
}

impl HttpTransport {
    pub fn new(token: Option<String>) -> Self {
        Self {
            api_base: "https://api.github.com".into(),
            token,
            timeout: Duration::from_secs(15),
        }
    }
}

impl Transport for HttpTransport {
    fn name(&self) -> &'static str {
        "http"
    }

    fn get(&self, _request: &ApiRequest) -> Result<ApiResponse> {
        Err(not_implemented("http transport"))
    }

    fn graphql(&self, _query: &str, _variables: &Value) -> Result<ApiResponse> {
        Err(not_implemented("http transport"))
    }
}
