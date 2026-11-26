//! Cross-process coordination, all on SQLite plus file locks (§4, §9).
//!
//! - single-flight: one process fetches a key, others wait briefly and read its result;
//! - shared quota: every response's `x-ratelimit-*` is recorded, and requests must
//!   `reserve` first (refused at or below the reserve, or when the global window is full);
//! - claim leases: local, lease-based issue claims (default 48h).

use crate::error::{not_implemented, Result};
use crate::models::{AgentId, ClaimOutcome, IssueRef};
use crate::store::SharedStore;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs::File;
use std::str::FromStr;
use std::time::Duration;

/// A GitHub rate-limit bucket.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Resource {
    Core,
    Search,
    Graphql,
}

impl Resource {
    pub const ALL: [Resource; 3] = [Resource::Core, Resource::Search, Resource::Graphql];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Core => "core",
            Self::Search => "search",
            Self::Graphql => "graphql",
        }
    }
}

impl fmt::Display for Resource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Resource {
    type Err = String;

    /// Also accepts GitHub's `x-ratelimit-resource` values.
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "core" => Ok(Self::Core),
            "search" => Ok(Self::Search),
            "graphql" => Ok(Self::Graphql),
            other => Err(format!("unknown rate-limit resource {other:?}")),
        }
    }
}

/// Last observed `x-ratelimit-*` values for one resource (table `quota`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotaSnapshot {
    pub resource: Resource,
    pub limit: u32,
    pub remaining: u32,
    pub reset_at: DateTime<Utc>,
    pub observed_at: DateTime<Utc>,
}

/// Key for single-flight locks and `http_cache`: a URL, or repo plus data kind.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CacheKey(String);

impl CacheKey {
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Held while this process fetches a key; dropping it releases the lock.
/// `file` is `None` for in-memory stores (tests), where locking is a no-op.
#[derive(Debug)]
pub struct FlightGuard {
    pub key: CacheKey,
    #[allow(dead_code)] // held for its Drop
    pub(crate) file: Option<File>,
}

/// Permission to send `n` requests against `resource`, already counted in
/// `request_log` so other processes see it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reservation {
    pub resource: Resource,
    pub n: u32,
    pub granted_at: DateTime<Utc>,
}

pub trait Coordinator: Send + Sync {
    /// Returns a guard if this process should fetch `key`. Returns `Ok(None)` if
    /// another process held the lock for the whole `wait`; the caller then reads
    /// what that process stored (or reports `partial`).
    fn single_flight(&self, key: &CacheKey, wait: Duration) -> Result<Option<FlightGuard>>;

    /// Reserves shared quota. Fails with `ScoutError::QuotaReserved` when the last
    /// observed remaining is at or below the configured reserve (until `reset_at`),
    /// or `ScoutError::WindowLimit` when the all-agent window is full.
    fn reserve(&self, resource: Resource, n: u32) -> Result<Reservation>;

    /// Records the `x-ratelimit-*` headers of a response.
    fn observe(&self, snapshot: &QuotaSnapshot) -> Result<()>;

    /// Acquires the claim, renews it if `agent` already holds it, or reports the holder.
    /// Expired leases are released automatically.
    fn claim(&self, issue: &IssueRef, agent: &AgentId, lease: Duration) -> Result<ClaimOutcome>;

    /// Releases `agent`'s active claim, if any.
    fn release(&self, issue: &IssueRef, agent: &AgentId) -> Result<()>;
}

/// The SQLite-backed coordinator; every agent on the machine shares its database.
pub struct SqliteCoordinator {
    pub store: SharedStore,
    pub agent: AgentId,
    pub reserves: crate::config::QuotaReserves,
    pub windows: crate::config::WindowLimits,
}

impl Coordinator for SqliteCoordinator {
    fn single_flight(&self, _key: &CacheKey, _wait: Duration) -> Result<Option<FlightGuard>> {
        Err(not_implemented("single-flight locks"))
    }

    fn reserve(&self, _resource: Resource, _n: u32) -> Result<Reservation> {
        Err(not_implemented("shared quota reservation"))
    }

    fn observe(&self, _snapshot: &QuotaSnapshot) -> Result<()> {
        Err(not_implemented("shared quota recording"))
    }

    fn claim(&self, _issue: &IssueRef, _agent: &AgentId, _lease: Duration) -> Result<ClaimOutcome> {
        Err(not_implemented("claims"))
    }

    fn release(&self, _issue: &IssueRef, _agent: &AgentId) -> Result<()> {
        Err(not_implemented("claims"))
    }
}
