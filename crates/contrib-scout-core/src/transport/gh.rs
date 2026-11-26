//! Default transport: `gh api -i` subprocess (§2, §11.1).
//!
//! Requirements for the implementation (§2, §13):
//! - Parse the status line from `-i` output; do NOT trust the exit code —
//!   `gh` exits 1 on 304 and prints `gh: HTTP 304` to stderr.
//! - Accept both `HTTP/1.1 200 OK` and `HTTP/2.0 200` status lines.
//! - Pass weak ETags (`W/"…"`) back verbatim in `If-None-Match`.
//! - Never use `gh api --cache`; caching is the store's job.
//! - A missing `gh` binary or spawn failure is `ScoutError::NetworkUnavailable`.

use super::{ApiRequest, ApiResponse, Transport};
use crate::error::{not_implemented, Result};
use serde_json::Value;
use std::path::PathBuf;

pub struct GhTransport {
    /// The `gh` executable (config `gh_path`).
    pub gh: PathBuf,
    /// GitHub host, `github.com` unless GHES.
    pub hostname: String,
}

impl GhTransport {
    pub fn new(gh: impl Into<PathBuf>) -> Self {
        Self {
            gh: gh.into(),
            hostname: "github.com".into(),
        }
    }
}

impl Transport for GhTransport {
    fn name(&self) -> &'static str {
        "gh"
    }

    fn get(&self, _request: &ApiRequest) -> Result<ApiResponse> {
        Err(not_implemented("gh transport"))
    }

    fn graphql(&self, _query: &str, _variables: &Value) -> Result<ApiResponse> {
        Err(not_implemented("gh transport"))
    }
}
