//! Cross-process coordination, all on SQLite plus file locks (§4, §9).
//!
//! - single-flight: one process fetches a key, others wait briefly and read its result;
//! - shared quota: every response's `x-ratelimit-*` is recorded, and requests must
//!   `reserve` first (refused at or below the reserve, or when the global window is full);
//! - claim leases: local, lease-based issue claims (default 48h).

use crate::config::{Config, QuotaReserves, WindowLimits};
use crate::error::Result;
use crate::models::{AgentId, ClaimOutcome, IssueRef};
use crate::store::{ScopeLock, SharedStore, Store};
use crate::transport::{ApiRequest, ApiResponse};
use chrono::{DateTime, TimeDelta, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;
use std::sync::MutexGuard;
use std::thread;
use std::time::{Duration, Instant};

/// The REST origin every request must stay on.
pub const API_ORIGIN: &str = "https://api.github.com";

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

    /// The bucket a REST path counts against: `/search/*` → search,
    /// `/graphql` → graphql, anything else → core. Absolute same-origin URLs
    /// are judged by their path.
    pub fn for_path(path: &str) -> Self {
        let path = path.strip_prefix(API_ORIGIN).unwrap_or(path);
        let path = path.split(['?', '#']).next().unwrap_or(path);
        if path == "/search" || path.starts_with("/search/") {
            Self::Search
        } else if path == "/graphql" {
            Self::Graphql
        } else {
            Self::Core
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

impl QuotaSnapshot {
    /// Parses `x-ratelimit-limit`, `-remaining`, `-reset` (Unix seconds), and
    /// `-resource`. `fallback` is used when `x-ratelimit-resource` is absent.
    /// Returns `None` when a header is missing or malformed, or when the
    /// resource is one we do not track (e.g. `code_search`).
    pub fn from_headers(
        response: &ApiResponse,
        fallback: Resource,
        observed_at: DateTime<Utc>,
    ) -> Option<Self> {
        let number = |name: &str| response.header_value(name)?.trim().parse::<u64>().ok();
        let resource = match response.header_value("x-ratelimit-resource") {
            Some(name) => name.trim().parse().ok()?,
            None => fallback,
        };
        Some(Self {
            resource,
            limit: u32::try_from(number("x-ratelimit-limit")?).ok()?,
            remaining: u32::try_from(number("x-ratelimit-remaining")?).ok()?,
            reset_at: DateTime::from_timestamp(
                i64::try_from(number("x-ratelimit-reset")?).ok()?,
                0,
            )?,
            observed_at,
        })
    }
}

/// Key for single-flight locks and `http_cache`: a URL, or repo plus data kind.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CacheKey(String);

impl CacheKey {
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    /// The default key of a REST GET: `GET <path>` with the API origin stripped
    /// (so a relative path and its absolute `Link` form share one entry), plus
    /// the `Accept` header when it is not the default.
    pub fn for_request(request: &ApiRequest) -> Self {
        let path = request
            .path
            .strip_prefix(API_ORIGIN)
            .unwrap_or(&request.path);
        match &request.accept {
            Some(accept) => Self(format!("GET {path} accept={accept}")),
            None => Self(format!("GET {path}")),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CacheKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Held while this process fetches a key; dropping it releases the lock.
/// For in-memory stores (tests) the lock is a no-op.
pub struct FlightGuard {
    pub key: CacheKey,
    _lock: ScopeLock,
}

impl fmt::Debug for FlightGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FlightGuard")
            .field("key", &self.key)
            .finish()
    }
}

/// Permission to send `n` requests against `resource`, already counted in
/// `request_log` so other processes see it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reservation {
    pub resource: Resource,
    pub n: u32,
    pub granted_at: DateTime<Utc>,
    /// `request_log` rows to settle with the final status.
    #[serde(skip)]
    pub log_ids: Vec<i64>,
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

    /// Records the final HTTP status of a reservation's requests in
    /// `request_log` (`None` when no response arrived). Default: nothing.
    fn settle(&self, _reservation: &Reservation, _status: Option<u16>) -> Result<()> {
        Ok(())
    }

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

/// How often single-flight re-polls a contended lock file.
const FLIGHT_POLL: Duration = Duration::from_millis(25);

impl SqliteCoordinator {
    pub fn new(
        store: SharedStore,
        agent: AgentId,
        reserves: QuotaReserves,
        windows: WindowLimits,
    ) -> Self {
        Self {
            store,
            agent,
            reserves,
            windows,
        }
    }

    /// Reserves and windows from `config`.
    pub fn from_config(store: SharedStore, agent: AgentId, config: &Config) -> Self {
        Self::new(store, agent, config.reserves(), config.windows())
    }

    fn store(&self) -> MutexGuard<'_, Store> {
        self.store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn lease_end(now: DateTime<Utc>, lease: Duration) -> DateTime<Utc> {
    TimeDelta::from_std(lease)
        .ok()
        .and_then(|lease| now.checked_add_signed(lease))
        .unwrap_or(DateTime::<Utc>::MAX_UTC)
}

impl Coordinator for SqliteCoordinator {
    /// Polls the key's lock file (`Store::lock_scope`) without holding the store
    /// mutex while waiting, so other threads of this process can keep using the
    /// database — including the thread that is fetching this key.
    fn single_flight(&self, key: &CacheKey, wait: Duration) -> Result<Option<FlightGuard>> {
        let scope = format!("flight:{}", key.as_str());
        let start = Instant::now();
        loop {
            if let Some(lock) = self.store().lock_scope(&scope, Duration::ZERO)? {
                return Ok(Some(FlightGuard {
                    key: key.clone(),
                    _lock: lock,
                }));
            }
            let elapsed = start.elapsed();
            if elapsed >= wait {
                return Ok(None);
            }
            thread::sleep(FLIGHT_POLL.min(wait - elapsed));
        }
    }

    fn reserve(&self, resource: Resource, n: u32) -> Result<Reservation> {
        let now = Utc::now();
        let log_ids = self.store().reserve_requests(
            resource,
            n,
            Some(&self.agent),
            self.reserves.get(resource),
            self.windows,
            now,
        )?;
        Ok(Reservation {
            resource,
            n,
            granted_at: now,
            log_ids,
        })
    }

    fn settle(&self, reservation: &Reservation, status: Option<u16>) -> Result<()> {
        self.store().settle_requests(&reservation.log_ids, status)
    }

    fn observe(&self, snapshot: &QuotaSnapshot) -> Result<()> {
        self.store().record_quota(snapshot)
    }

    fn claim(&self, issue: &IssueRef, agent: &AgentId, lease: Duration) -> Result<ClaimOutcome> {
        let now = Utc::now();
        self.store()
            .claim_issue(issue, agent, now, lease_end(now, lease), None)
    }

    fn release(&self, issue: &IssueRef, agent: &AgentId) -> Result<()> {
        self.store().release_claim(issue, agent, Utc::now())?;
        Ok(())
    }
}
