//! `http_cache`, `discover_runs`, and whole-database stats / prune (§6, §7).
//! Step 2 owns the HTTP cache; step 4 the discover runs.

use super::{Cached, Store};
use crate::error::{not_implemented, Result};
use crate::models::RepoName;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// A stored response, revalidated with `etag` / `last_modified`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CachedResponse {
    pub key: String,
    pub status: u16,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// A cached search result (TTL 24h, discovery only).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DiscoverRun {
    pub query_key: String,
    pub query: String,
    pub params: Value,
    pub repos: Vec<RepoName>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheStats {
    /// Row count per table.
    pub rows: BTreeMap<String, u64>,
    pub bytes: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PruneStats {
    /// Rows deleted per table. Policy marks and history are never pruned.
    pub deleted: BTreeMap<String, u64>,
}

impl Store {
    pub fn cached_response(&self, _key: &str) -> Result<Option<Cached<CachedResponse>>> {
        Err(not_implemented("http cache"))
    }

    pub fn save_response(&mut self, _response: &CachedResponse, _at: DateTime<Utc>) -> Result<()> {
        Err(not_implemented("http cache"))
    }

    /// Updates `fetched_at` after a 304.
    pub fn touch_response(&mut self, _key: &str, _at: DateTime<Utc>) -> Result<()> {
        Err(not_implemented("http cache"))
    }

    pub fn discover_run(&self, _query_key: &str) -> Result<Option<Cached<DiscoverRun>>> {
        Err(not_implemented("discover cache"))
    }

    pub fn save_discover_run(&mut self, _run: &DiscoverRun, _at: DateTime<Utc>) -> Result<()> {
        Err(not_implemented("discover cache"))
    }

    pub fn stats(&self) -> Result<CacheStats> {
        Err(not_implemented("cache stats"))
    }

    /// Deletes cached data fetched before `older_than`. Never touches
    /// `policy_marks`, `policy_mark_history`, active claims, or `tracked_repos`.
    pub fn prune(&mut self, _older_than: DateTime<Utc>) -> Result<PruneStats> {
        Err(not_implemented("cache prune"))
    }
}
