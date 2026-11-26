//! Request pipeline shared by every resource (§4):
//! budget → shared quota (`Coordinator::reserve`) → token bucket → send →
//! classify (2xx / 304 / retryable / fatal) → retry. Every response's
//! `x-ratelimit-*` headers go to `Coordinator::observe`.
//!
//! Adapted from dwave `wave-core/src/gateway.rs` (host policy, token bucket,
//! retries, per-call budget, counters).

use crate::coordinator::Coordinator;
use crate::error::{not_implemented, Result, ScoutError};
use crate::freshness::{Live, LiveRead};
use crate::transport::{ApiRequest, ApiResponse, Transport};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, Copy, Debug)]
pub struct HostPolicy {
    pub rate_per_second: f64,
    pub burst: u32,
    pub max_attempts: u32,
    pub retry_base: Duration,
    pub max_retry_wait: Duration,
    pub honor_retry_after: bool,
}

impl Default for HostPolicy {
    fn default() -> Self {
        Self {
            rate_per_second: 2.0,
            burst: 4,
            max_attempts: 3,
            retry_base: Duration::from_millis(500),
            max_retry_wait: Duration::from_secs(30),
            honor_retry_after: true,
        }
    }
}

/// Counters reported in `meta.requests` / `meta.served_304` and `-v`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Metrics {
    /// Requests sent, including retries and 304s.
    pub requests: u32,
    pub retries: u32,
    pub served_304: u32,
    pub bytes: u64,
}

/// Per-invocation request cap (`--max-requests` / config `max_requests`).
/// Every attempt, retries included, spends one unit.
#[derive(Debug)]
pub struct Budget {
    limit: u32,
    used: AtomicU32,
}

impl Budget {
    pub fn new(limit: u32) -> Self {
        Self {
            limit,
            used: AtomicU32::new(0),
        }
    }

    pub fn limit(&self) -> u32 {
        self.limit
    }

    pub fn used(&self) -> u32 {
        self.used.load(Ordering::SeqCst)
    }

    pub fn remaining(&self) -> u32 {
        self.limit.saturating_sub(self.used())
    }

    /// Spends one unit or fails with [`ScoutError::BudgetExhausted`].
    pub fn try_spend(&self) -> Result<(), ScoutError> {
        self.used
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |used| {
                (used < self.limit).then_some(used + 1)
            })
            .map(|_| ())
            .map_err(|_| ScoutError::BudgetExhausted { limit: self.limit })
    }
}

/// How the gateway treats a response status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disposition {
    Success,
    NotModified,
    /// 429, 5xx, or 403 with rate-limit / `retry-after` signals.
    Retryable,
    /// Anything else (including 404, which callers may treat as "absent").
    Fatal,
}

pub fn classify(response: &ApiResponse) -> Disposition {
    match response.status {
        200..=299 => Disposition::Success,
        304 => Disposition::NotModified,
        429 | 500 | 502 | 503 | 504 => Disposition::Retryable,
        403 if response.header_value("retry-after").is_some()
            || response.header_value("x-ratelimit-remaining") == Some("0") =>
        {
            Disposition::Retryable
        }
        _ => Disposition::Fatal,
    }
}

pub struct Gateway {
    pub transport: Arc<dyn Transport>,
    pub policy: HostPolicy,
    pub budget: Budget,
    pub coordinator: Option<Arc<dyn Coordinator>>,
    metrics: Mutex<Metrics>,
}

impl Gateway {
    pub fn new(transport: Arc<dyn Transport>, policy: HostPolicy, budget: Budget) -> Self {
        Self {
            transport,
            policy,
            budget,
            coordinator: None,
            metrics: Mutex::new(Metrics::default()),
        }
    }

    pub fn with_coordinator(mut self, coordinator: Arc<dyn Coordinator>) -> Self {
        self.coordinator = Some(coordinator);
        self
    }

    pub fn metrics(&self) -> Metrics {
        *self
            .metrics
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// One logical GET through the full pipeline. Returns 2xx/304/404 responses;
    /// other final statuses become `ScoutError::HttpStatus`.
    pub fn get(&self, _request: &ApiRequest) -> Result<ApiResponse> {
        Err(not_implemented("gateway"))
    }

    /// Like [`Gateway::get`] but the result is proof of a live response (200 or
    /// 304). Never served from the cache; network failure is an error (§4, §8).
    pub fn get_live(&self, _request: &ApiRequest, _live: LiveRead) -> Result<Live<ApiResponse>> {
        Err(not_implemented("gateway"))
    }

    pub fn graphql(&self, _query: &str, _variables: &Value) -> Result<ApiResponse> {
        Err(not_implemented("gateway"))
    }
}
