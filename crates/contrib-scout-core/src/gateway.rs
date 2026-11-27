//! Request pipeline shared by every resource (§4):
//! budget → shared quota (`Coordinator::reserve`) → token bucket → send →
//! classify (2xx / 304 / retryable / fatal) → retry. Every response's
//! `x-ratelimit-*` headers go to `Coordinator::observe`.
//!
//! Adapted from dwave `wave-core/src/gateway.rs` (host policy, token bucket,
//! retries, per-call budget, counters).
//!
//! Three entry points:
//! - [`Gateway::get`] / [`Gateway::get_keyed`]: conditional, cached reads for
//!   `Offline` / `Default` / `Fresh`, with cross-process single-flight;
//! - [`Gateway::get_live`]: always a network response (304 counts), never the
//!   cache as a fallback — the only way to obtain a [`Live`] response;
//! - [`Gateway::send`] / [`Gateway::graphql`]: the bare pipeline, no cache.
//!
//! Choices:
//! - Only GETs are retried (max `max_attempts`, default 3; exponential backoff
//!   with full jitter). `Retry-After` and, for exhausted quota,
//!   `x-ratelimit-reset` replace the backoff; a wait longer than
//!   `max_retry_wait` fails immediately instead of sleeping. Transport errors
//!   ([`ScoutError::NetworkUnavailable`], e.g. timeouts) are retried too.
//! - A full all-agent *per-minute* window gets one bounded wait
//!   ([`Gateway::window_wait`], default 3s) and one more reservation attempt;
//!   a full per-hour window, or a quota at its reserve, fails immediately.
//! - `Default` / `Fresh` reads fall back to a cached copy (with
//!   [`Fetched::stale`] set) when the fetch fails for network, budget, quota,
//!   window, 403, 429 or 5xx reasons. `Live` never falls back.
//! - 2xx and 404 (negative cache) responses are cached; 304s refresh
//!   `fetched_at`; 403 / 5xx are never cached.

use crate::coordinator::{CacheKey, Coordinator, QuotaSnapshot, Reservation, Resource, API_ORIGIN};
use crate::error::{Result, ScoutError};
use crate::freshness::{Freshness, Live, LiveRead, Origin};
use crate::store::{Cached, CachedResponse, SharedStore, Store};
use crate::transport::{ApiRequest, ApiResponse, Transport};
use anyhow::{anyhow, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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

impl HostPolicy {
    /// The search bucket: GitHub allows 30 requests per minute, so one request
    /// every two seconds with no burst (§2). The all-agent cap is the separate
    /// `window_search_per_minute` window.
    pub fn search() -> Self {
        Self {
            rate_per_second: 0.5,
            burst: 1,
            ..Self::default()
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
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |used| {
                (used < self.limit).then_some(used + 1)
            })
            .map(|_| ())
            .map_err(|_| ScoutError::BudgetExhausted { limit: self.limit })
    }

    /// Returns a unit spent on a request that was never sent (refused reservation).
    fn refund(&self) {
        let _ = self
            .used
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |used| {
                used.checked_sub(1)
            });
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

/// Sleeps for a duration; injectable so tests run without waiting.
pub type SleepFn = Arc<dyn Fn(Duration) + Send + Sync>;

/// Result of a cached read ([`Gateway::get`]).
#[derive(Clone, Debug, PartialEq)]
pub struct Fetched {
    pub key: CacheKey,
    /// A 2xx or 404 response. After a 304 this is the cached response
    /// (status 200, cached body and headers). The only exception: a 304 to a
    /// caller-supplied `If-None-Match` with nothing cached under `key` is
    /// returned as-is (status 304, empty body).
    pub response: ApiResponse,
    pub origin: Origin,
    /// When the body was last confirmed by GitHub.
    pub fetched_at: DateTime<Utc>,
    /// Set when the fetch failed and a cached copy was served instead; the
    /// caller should add it to `meta.warnings` and set `meta.partial`.
    pub stale: Option<String>,
}

impl Fetched {
    pub fn is_not_found(&self) -> bool {
        self.response.status == 404
    }
}

/// Per-process token bucket (debt model: a caller takes its token immediately
/// and sleeps off any deficit, so one sleep always suffices).
struct Bucket {
    tokens: f64,
    updated: Instant,
    /// Set by `Retry-After` / `x-ratelimit-reset`; later requests wait for it.
    retry_at: Option<(Instant, u16)>,
}

impl Bucket {
    fn new(policy: HostPolicy, now: Instant) -> Self {
        Self {
            tokens: f64::from(policy.burst),
            updated: now,
            retry_at: None,
        }
    }

    fn acquire(&mut self, policy: HostPolicy, now: Instant) -> Duration {
        let elapsed = now.saturating_duration_since(self.updated).as_secs_f64();
        self.tokens = (self.tokens + elapsed * policy.rate_per_second).min(f64::from(policy.burst));
        self.updated = now;
        self.tokens -= 1.0;
        if self.tokens >= 0.0 {
            Duration::ZERO
        } else {
            Duration::try_from_secs_f64(-self.tokens / policy.rate_per_second)
                .unwrap_or(policy.max_retry_wait)
        }
    }
}

pub struct Gateway {
    pub transport: Arc<dyn Transport>,
    /// Core and GraphQL policy.
    pub policy: HostPolicy,
    /// Search policy (default [`HostPolicy::search`]).
    pub search_policy: HostPolicy,
    pub budget: Budget,
    pub coordinator: Option<Arc<dyn Coordinator>>,
    /// Store holding `http_cache`; without it every read goes to the network
    /// and `Offline` reads fail.
    pub cache: Option<SharedStore>,
    /// How long single-flight waits for another process's fetch (config `lock_wait`).
    pub lock_wait: Duration,
    /// One bounded wait when the all-agent per-minute window is full.
    pub window_wait: Duration,
    sleep: SleepFn,
    metrics: Mutex<Metrics>,
    buckets: Mutex<HashMap<Resource, Bucket>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Gateway {
    pub fn new(transport: Arc<dyn Transport>, policy: HostPolicy, budget: Budget) -> Self {
        Self {
            transport,
            policy,
            search_policy: HostPolicy::search(),
            budget,
            coordinator: None,
            cache: None,
            lock_wait: Duration::from_secs(10),
            window_wait: Duration::from_secs(3),
            sleep: Arc::new(std::thread::sleep),
            metrics: Mutex::new(Metrics::default()),
            buckets: Mutex::new(HashMap::new()),
        }
    }

    pub fn with_coordinator(mut self, coordinator: Arc<dyn Coordinator>) -> Self {
        self.coordinator = Some(coordinator);
        self
    }

    /// Enables the conditional HTTP cache (`http_cache`) in `store`.
    pub fn with_cache(mut self, store: SharedStore) -> Self {
        self.cache = Some(store);
        self
    }

    pub fn with_lock_wait(mut self, wait: Duration) -> Self {
        self.lock_wait = wait;
        self
    }

    pub fn with_window_wait(mut self, wait: Duration) -> Self {
        self.window_wait = wait;
        self
    }

    pub fn with_search_policy(mut self, policy: HostPolicy) -> Self {
        self.search_policy = policy;
        self
    }

    /// Replaces `std::thread::sleep` for backoff, `Retry-After`, token-bucket
    /// and window waits (tests pass a recorder that does not sleep).
    pub fn with_sleep(mut self, sleep: SleepFn) -> Self {
        self.sleep = sleep;
        self
    }

    pub fn metrics(&self) -> Metrics {
        *lock(&self.metrics)
    }

    fn policy_for(&self, resource: Resource) -> HostPolicy {
        match resource {
            Resource::Search => self.search_policy,
            Resource::Core | Resource::Graphql => self.policy,
        }
    }

    // ------------------------------------------------------------------
    // Cached reads
    // ------------------------------------------------------------------

    /// A cached, conditional GET keyed by [`CacheKey::for_request`].
    /// `ttl` is the data's `Default` TTL (§7, `Config::ttls`). See
    /// [`Gateway::get_keyed`].
    pub fn get(
        &self,
        request: &ApiRequest,
        freshness: Freshness,
        ttl: Duration,
    ) -> Result<Fetched> {
        self.get_keyed(&CacheKey::for_request(request), request, freshness, ttl)
    }

    /// A cached, conditional GET stored under `key`:
    /// - `Offline`: the cached copy, or [`ScoutError::NotFound`] if none;
    /// - `Default`: the cached copy if younger than `ttl`, else a conditional request;
    /// - `Fresh`: a conditional request regardless of age;
    /// - `Live`: as [`Gateway::get_live`] (no fallback).
    ///
    /// Network reads run under the key's single-flight lock: a process that
    /// waited for another one's fetch re-reads the cache and uses that result
    /// instead of fetching again.
    pub fn get_keyed(
        &self,
        key: &CacheKey,
        request: &ApiRequest,
        freshness: Freshness,
        ttl: Duration,
    ) -> Result<Fetched> {
        if let Some(live) = freshness.live() {
            let live = self.get_live_keyed(key, request, live)?;
            let origin = if live.not_modified() {
                Origin::NotModified
            } else {
                Origin::Network
            };
            let fetched_at = live.as_of();
            return Ok(Fetched {
                key: key.clone(),
                response: live.into_inner(),
                origin,
                fetched_at,
                stale: None,
            });
        }

        let cached = self.read_cache(key)?;
        if freshness == Freshness::Offline {
            return cached
                .map(|cached| from_cache(key, cached, None))
                .ok_or_else(|| {
                    ScoutError::NotFound {
                        what: format!("cached response for {key} (offline)"),
                    }
                    .into()
                });
        }
        if let Some(hit) = cached.as_ref().filter(|c| !needs_fetch(c, freshness, ttl)) {
            return Ok(from_cache(key, hit.clone(), None));
        }

        let mut guard = None;
        let mut cached = cached;
        if let (Some(coordinator), Some(_)) = (&self.coordinator, &self.cache) {
            let before = cached.as_ref().map(|c| c.fetched_at);
            guard = coordinator.single_flight(key, self.lock_wait)?;
            let after = self.read_cache(key)?;
            let written_meanwhile = after.as_ref().map(|c| c.fetched_at) != before;
            match (&guard, after) {
                // Another process refreshed it while we waited: use its result.
                (_, Some(peer))
                    if written_meanwhile
                        && (freshness == Freshness::Fresh
                            || !needs_fetch(&peer, freshness, ttl)) =>
                {
                    return Ok(from_cache(key, peer, None));
                }
                // Still locked after `lock_wait`: serve what we have rather than pile on.
                (None, Some(stale)) => {
                    let note = format!(
                        "another agent is still fetching {key}; served the cached copy from {}",
                        stale.fetched_at.to_rfc3339()
                    );
                    return Ok(from_cache(key, stale, Some(note)));
                }
                (_, after) => cached = after,
            }
        }

        let result = self.fetch_conditional(key, request, cached.as_ref());
        drop(guard);
        match (result, cached) {
            (Ok(fetched), _) => Ok(fetched),
            (Err(error), Some(cached))
                if freshness.may_fall_back_to_cache() && may_serve_stale(&error) =>
            {
                let note = format!(
                    "fetch failed ({error:#}); served the cached copy of {key} from {}",
                    cached.fetched_at.to_rfc3339()
                );
                Ok(from_cache(key, cached, Some(note)))
            }
            (Err(error), _) => Err(error),
        }
    }

    /// Like [`Gateway::get`] but the result is proof of a live response (2xx,
    /// 304, or 404). Never served from the cache; network failure is an error
    /// (§4, §8). The cache only supplies the `If-None-Match` validator; a 304
    /// returns the cached body with `not_modified() == true`.
    pub fn get_live(&self, request: &ApiRequest, live: LiveRead) -> Result<Live<ApiResponse>> {
        self.get_live_keyed(&CacheKey::for_request(request), request, live)
    }

    /// [`Gateway::get_live`] with an explicit cache key.
    pub fn get_live_keyed(
        &self,
        key: &CacheKey,
        request: &ApiRequest,
        _live: LiveRead,
    ) -> Result<Live<ApiResponse>> {
        let cached = self.read_cache(key)?;
        let fetched = self.fetch_conditional(key, request, cached.as_ref())?;
        let not_modified = fetched.origin == Origin::NotModified;
        Ok(Live::new(
            fetched.response,
            fetched.fetched_at,
            not_modified,
        ))
    }

    fn read_cache(&self, key: &CacheKey) -> Result<Option<Cached<CachedResponse>>> {
        match &self.cache {
            Some(store) => lock(store).cached_response(key.as_str()),
            None => Ok(None),
        }
    }

    fn with_store<T>(&self, f: impl FnOnce(&mut Store) -> Result<T>) -> Result<Option<T>> {
        match &self.cache {
            Some(store) => f(&mut lock(store)).map(Some),
            None => Ok(None),
        }
    }

    /// Sends `request` with validators from `cached`, then updates the cache.
    fn fetch_conditional(
        &self,
        key: &CacheKey,
        request: &ApiRequest,
        cached: Option<&Cached<CachedResponse>>,
    ) -> Result<Fetched> {
        let mut request = request.clone();
        // Only a cached 2xx body can be revalidated; a 404 has nothing to reuse.
        let validators = cached.filter(|c| (200..300).contains(&c.data.status));
        if let Some(cached) = validators {
            if request.if_none_match.is_none() {
                request.if_none_match.clone_from(&cached.data.etag);
            }
            if let (None, Some(modified)) = (&request.if_none_match, &cached.data.last_modified) {
                request
                    .headers
                    .push(("if-modified-since".into(), modified.clone()));
            }
        }
        let response = self.send(&request)?;
        let now = Utc::now();
        if response.is_not_modified() {
            let Some(cached) = validators else {
                // The caller's own validator: hand the 304 back untouched.
                return Ok(Fetched {
                    key: key.clone(),
                    response,
                    origin: Origin::NotModified,
                    fetched_at: now,
                    stale: None,
                });
            };
            let mut data = cached.data.clone();
            match response.etag() {
                Some(etag) if Some(etag) != data.etag.as_deref() => {
                    data.etag = Some(etag.to_owned());
                    self.with_store(|store| store.save_response(&data, now))?;
                }
                _ => {
                    self.with_store(|store| store.touch_response(key.as_str(), now))?;
                }
            }
            return Ok(Fetched {
                key: key.clone(),
                response: to_response(&data),
                origin: Origin::NotModified,
                fetched_at: now,
                stale: None,
            });
        }
        // `send` only returns 2xx, 304, or 404.
        let entry = CachedResponse {
            key: key.as_str().to_owned(),
            status: response.status,
            etag: response.etag().map(str::to_owned),
            last_modified: response.header_value("last-modified").map(str::to_owned),
            headers: response
                .headers
                .iter()
                .filter(|(name, _)| !name.starts_with("x-ratelimit-"))
                .cloned()
                .collect(),
            body: response.body.clone(),
        };
        self.with_store(|store| store.save_response(&entry, now))?;
        Ok(Fetched {
            key: key.clone(),
            response,
            origin: Origin::Network,
            fetched_at: now,
            stale: None,
        })
    }

    // ------------------------------------------------------------------
    // Bare pipeline
    // ------------------------------------------------------------------

    /// One logical GET through the full pipeline, without the cache. Returns
    /// 2xx / 304 / 404 responses; other final statuses become
    /// [`ScoutError::HttpStatus`]. The resource is inferred from the path
    /// ([`Resource::for_path`]) unless the request names a non-core one.
    pub fn send(&self, request: &ApiRequest) -> Result<ApiResponse> {
        check_origin(&request.path)?;
        let resource = match request.resource {
            Resource::Core => Resource::for_path(&request.path),
            explicit => explicit,
        };
        let url = absolute_url(&request.path);
        self.execute(resource, true, &url, || self.transport.get(request))
    }

    /// One GraphQL POST through the pipeline. Never retried (POST); the body
    /// may still carry GraphQL `errors` for the caller to inspect.
    pub fn graphql(&self, query: &str, variables: &Value) -> Result<ApiResponse> {
        let url = format!("{API_ORIGIN}/graphql");
        self.execute(Resource::Graphql, false, &url, || {
            self.transport.graphql(query, variables)
        })
    }

    fn execute(
        &self,
        resource: Resource,
        retry: bool,
        url: &str,
        send: impl Fn() -> Result<ApiResponse>,
    ) -> Result<ApiResponse> {
        let policy = self.policy_for(resource);
        let attempts = if retry { policy.max_attempts.max(1) } else { 1 };
        let mut attempt = 0;
        loop {
            self.budget.try_spend()?;
            let reservation = match self.reserve(resource) {
                Ok(reservation) => reservation,
                Err(error) => {
                    self.budget.refund();
                    return Err(error);
                }
            };
            if let Err(error) = self.throttle(resource, policy, url) {
                // Nothing was sent: give the budget unit back and close the log row.
                self.budget.refund();
                if let (Some(coordinator), Some(reservation)) = (&self.coordinator, &reservation) {
                    coordinator.settle(reservation, None)?;
                }
                return Err(error);
            }
            {
                let mut metrics = lock(&self.metrics);
                metrics.requests += 1;
                metrics.retries += u32::from(attempt > 0);
            }
            let result = send();
            let now = Utc::now();
            if let Some(coordinator) = &self.coordinator {
                let status = result.as_ref().ok().map(|response| response.status);
                if let Some(reservation) = &reservation {
                    coordinator.settle(reservation, status)?;
                }
                if let Some(snapshot) = result
                    .as_ref()
                    .ok()
                    .and_then(|response| QuotaSnapshot::from_headers(response, resource, now))
                {
                    coordinator.observe(&snapshot)?;
                }
            }
            attempt += 1;
            let last = attempt >= attempts;

            let response = match result {
                Ok(response) => response,
                Err(error) => {
                    let network = matches!(
                        ScoutError::find(&error),
                        Some(ScoutError::NetworkUnavailable { .. })
                    );
                    if !network || last {
                        return Err(error);
                    }
                    (self.sleep)(backoff(policy, attempt));
                    continue;
                }
            };
            {
                let mut metrics = lock(&self.metrics);
                metrics.bytes = metrics.bytes.saturating_add(response.body.len() as u64);
                metrics.served_304 += u32::from(response.is_not_modified());
            }
            match classify(&response) {
                Disposition::Success | Disposition::NotModified => return Ok(response),
                Disposition::Fatal if response.status == 404 => return Ok(response),
                Disposition::Fatal => {
                    return Err(ScoutError::HttpStatus {
                        status: response.status,
                        url: url.to_owned(),
                    }
                    .into())
                }
                Disposition::Retryable => {
                    let error = ScoutError::HttpStatus {
                        status: response.status,
                        url: url.to_owned(),
                    };
                    let delay = server_delay(&response, policy, SystemTime::now());
                    if last || delay.is_some_and(|delay| delay > policy.max_retry_wait) {
                        if let Some(delay) = delay {
                            self.defer(resource, delay, response.status);
                        }
                        return Err(error.into());
                    }
                    match delay {
                        // `throttle` sleeps it off before the next attempt.
                        Some(delay) => self.defer(resource, delay, response.status),
                        None => (self.sleep)(backoff(policy, attempt)),
                    }
                }
            }
        }
    }

    /// Shared quota and all-agent windows; one bounded wait on a full minute window.
    fn reserve(&self, resource: Resource) -> Result<Option<Reservation>> {
        let Some(coordinator) = &self.coordinator else {
            return Ok(None);
        };
        match coordinator.reserve(resource, 1) {
            Ok(reservation) => Ok(Some(reservation)),
            Err(error)
                if !self.window_wait.is_zero()
                    && matches!(
                        ScoutError::find(&error),
                        Some(ScoutError::WindowLimit {
                            window: "minute",
                            ..
                        })
                    ) =>
            {
                (self.sleep)(self.window_wait);
                coordinator.reserve(resource, 1).map(Some)
            }
            Err(error) => Err(error),
        }
    }

    /// Per-process token bucket plus any server-requested pause for `resource`.
    fn throttle(&self, resource: Resource, policy: HostPolicy, url: &str) -> Result<()> {
        let delay = {
            let mut buckets = lock(&self.buckets);
            let now = Instant::now();
            let bucket = buckets
                .entry(resource)
                .or_insert_with(|| Bucket::new(policy, now));
            let pause = match bucket.retry_at.take() {
                Some((until, status)) => {
                    let pause = until.saturating_duration_since(now);
                    if pause > policy.max_retry_wait {
                        bucket.retry_at = Some((until, status));
                        return Err(ScoutError::HttpStatus {
                            status,
                            url: url.to_owned(),
                        }
                        .into());
                    }
                    pause
                }
                None => Duration::ZERO,
            };
            pause.max(bucket.acquire(policy, now))
        };
        if !delay.is_zero() {
            (self.sleep)(delay);
        }
        Ok(())
    }

    fn defer(&self, resource: Resource, delay: Duration, status: u16) {
        let mut buckets = lock(&self.buckets);
        let now = Instant::now();
        let policy = self.policy_for(resource);
        let bucket = buckets
            .entry(resource)
            .or_insert_with(|| Bucket::new(policy, now));
        if let Some(until) = now.checked_add(delay) {
            if bucket.retry_at.is_none_or(|(current, _)| current < until) {
                bucket.retry_at = Some((until, status));
            }
        }
    }
}

fn needs_fetch(cached: &Cached<CachedResponse>, freshness: Freshness, ttl: Duration) -> bool {
    let age = (Utc::now() - cached.fetched_at)
        .to_std()
        .unwrap_or_default();
    freshness.needs_fetch(Some(age), ttl)
}

fn to_response(data: &CachedResponse) -> ApiResponse {
    ApiResponse {
        status: data.status,
        headers: data.headers.clone(),
        body: data.body.clone(),
    }
}

fn from_cache(key: &CacheKey, cached: Cached<CachedResponse>, stale: Option<String>) -> Fetched {
    Fetched {
        key: key.clone(),
        response: to_response(&cached.data),
        origin: Origin::Cache,
        fetched_at: cached.fetched_at,
        stale,
    }
}

/// Failures a `Default` / `Fresh` read may answer from the cache.
fn may_serve_stale(error: &anyhow::Error) -> bool {
    match ScoutError::find(error) {
        Some(
            ScoutError::NetworkUnavailable { .. }
            | ScoutError::BudgetExhausted { .. }
            | ScoutError::QuotaReserved { .. }
            | ScoutError::WindowLimit { .. },
        ) => true,
        Some(ScoutError::HttpStatus { status, .. }) => {
            matches!(status, 403 | 429) || *status >= 500
        }
        _ => false,
    }
}

/// Full jitter: uniform in `[0, min(base * 2^(attempt-1), max_retry_wait)]`.
fn backoff(policy: HostPolicy, attempt: u32) -> Duration {
    let ceiling = policy
        .retry_base
        .saturating_mul(2u32.saturating_pow(attempt.saturating_sub(1)))
        .min(policy.max_retry_wait);
    Duration::try_from_secs_f64(ceiling.as_secs_f64() * rand::random_range(0.0..=1.0))
        .unwrap_or(ceiling)
}

/// The wait the server asked for: `Retry-After` (seconds or HTTP date), and
/// for exhausted quota (`x-ratelimit-remaining: 0`) the time until
/// `x-ratelimit-reset`; the longer of the two.
fn server_delay(response: &ApiResponse, policy: HostPolicy, now: SystemTime) -> Option<Duration> {
    if !policy.honor_retry_after {
        return None;
    }
    let retry_after = response
        .header_value("retry-after")
        .and_then(|value| parse_retry_after(value, now));
    let reset = (response.header_value("x-ratelimit-remaining") == Some("0"))
        .then(|| response.header_value("x-ratelimit-reset"))
        .flatten()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .and_then(|seconds| UNIX_EPOCH.checked_add(Duration::from_secs(seconds)))
        .map(|reset| reset.duration_since(now).unwrap_or_default());
    retry_after.max(reset)
}

/// `Retry-After`: delay seconds or an HTTP date.
pub fn parse_retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    if let Ok(seconds) = value.trim().parse() {
        return Some(Duration::from_secs(seconds));
    }
    httpdate::parse_http_date(value.trim())
        .ok()
        .map(|date| date.duration_since(now).unwrap_or_default())
}

/// Relative paths must start with a single `/`; absolute URLs (from `Link`
/// headers) must be plain `https://api.github.com/...` with no credentials,
/// port, or fragment.
fn check_origin(path: &str) -> Result<()> {
    if path.starts_with('/') && !path.starts_with("//") {
        return Ok(());
    }
    let url =
        reqwest::Url::parse(path).map_err(|e| anyhow!("invalid request path {path:?}: {e}"))?;
    let same_origin = url.scheme() == "https"
        && url.host_str() == Some("api.github.com")
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none();
    if !same_origin {
        bail!("refusing to send a request outside {API_ORIGIN}: {path}");
    }
    Ok(())
}

fn absolute_url(path: &str) -> String {
    if path.starts_with('/') {
        format!("{API_ORIGIN}{path}")
    } else {
        path.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_bucket_allows_burst_then_charges_debt() {
        let policy = HostPolicy {
            rate_per_second: 2.0,
            burst: 2,
            ..HostPolicy::default()
        };
        let start = Instant::now();
        let mut bucket = Bucket::new(policy, start);
        assert_eq!(bucket.acquire(policy, start), Duration::ZERO);
        assert_eq!(bucket.acquire(policy, start), Duration::ZERO);
        assert_eq!(bucket.acquire(policy, start), Duration::from_millis(500));
        assert_eq!(bucket.acquire(policy, start), Duration::from_millis(1000));
        let later = start + Duration::from_secs(10);
        assert_eq!(bucket.acquire(policy, later), Duration::ZERO);
    }

    #[test]
    fn retry_after_supports_dates_and_seconds() {
        let start = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let date = httpdate::fmt_http_date(start + Duration::from_secs(42));
        assert_eq!(
            parse_retry_after(&date, start),
            Some(Duration::from_secs(42))
        );
        assert_eq!(
            parse_retry_after("42", start),
            Some(Duration::from_secs(42))
        );
        assert_eq!(parse_retry_after("soon", start), None);
    }

    #[test]
    fn backoff_stays_under_the_exponential_ceiling() {
        let policy = HostPolicy::default();
        for attempt in 1..6 {
            let ceiling = policy
                .retry_base
                .saturating_mul(1 << (attempt - 1))
                .min(policy.max_retry_wait);
            assert!(backoff(policy, attempt) <= ceiling);
        }
    }

    #[test]
    fn origin_check_rejects_foreign_hosts() {
        assert!(check_origin("/repos/o/r").is_ok());
        assert!(check_origin("https://api.github.com/repos/o/r?page=2").is_ok());
        assert!(check_origin("//evil.example/x").is_err());
        assert!(check_origin("https://evil.example/repos").is_err());
        assert!(check_origin("http://api.github.com/repos").is_err());
        assert!(check_origin("https://user@api.github.com/repos").is_err());
    }

    #[test]
    fn resource_is_inferred_from_path() {
        assert_eq!(Resource::for_path("/search/issues?q=x"), Resource::Search);
        assert_eq!(
            Resource::for_path("https://api.github.com/search/repositories"),
            Resource::Search
        );
        assert_eq!(Resource::for_path("/graphql"), Resource::Graphql);
        assert_eq!(Resource::for_path("/repos/o/search"), Resource::Core);
    }
}
