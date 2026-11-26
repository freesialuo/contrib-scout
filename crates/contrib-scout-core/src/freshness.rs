//! Freshness modes and the type-level "gates only use live data" rule (§4, §7).
//!
//! - [`Freshness`] is what ordinary reads take.
//! - [`LiveRead`] is a zero-sized proof that the caller asked for live data. Only
//!   [`Freshness::Live`] produces one ([`Freshness::live`] / [`Freshness::require_live`]).
//!   Gate-facing fetch APIs take a `LiveRead` instead of a `Freshness`, so gate code
//!   cannot pass `Default` or `Offline` by mistake.
//! - [`Live<T>`] wraps data that was confirmed by a network response (200 or 304).
//!   Only this crate can construct it (from the gateway), so a function that takes
//!   `Live<T>` cannot be fed cached data. [`crate::gate`] consumes only `Live<T>`.

use crate::error::ScoutError;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Freshness {
    /// Read the local cache only; never touch the network.
    Offline,
    /// Fetch when the cached copy is missing or older than its TTL.
    Default,
    /// Fetch regardless of TTL (conditional requests still apply).
    Fresh,
    /// A network response is mandatory (304 counts). If the network is
    /// unavailable this is an error; it never falls back to the cache.
    Live,
}

impl Freshness {
    /// Whether this mode may issue requests at all.
    pub fn allows_fetch(self) -> bool {
        !matches!(self, Self::Offline)
    }

    /// Whether a read should go to the network, given the cached copy's age
    /// (`None` = nothing cached) and the TTL for that data (§7 table).
    pub fn needs_fetch(self, cached_age: Option<Duration>, ttl: Duration) -> bool {
        match self {
            Self::Offline => false,
            Self::Default => cached_age.is_none_or(|age| age >= ttl),
            Self::Fresh | Self::Live => true,
        }
    }

    /// Whether a failed fetch may be answered from the cache (with a warning
    /// and `meta.partial`). Never for `Live`.
    pub fn may_fall_back_to_cache(self) -> bool {
        !matches!(self, Self::Live)
    }

    /// The live-read token, only for [`Freshness::Live`].
    pub fn live(self) -> Option<LiveRead> {
        matches!(self, Self::Live).then_some(LiveRead { _private: () })
    }

    /// Like [`Freshness::live`], but fails with [`ScoutError::LiveRequired`].
    pub fn require_live(self, what: &str) -> Result<LiveRead, ScoutError> {
        self.live()
            .ok_or_else(|| ScoutError::LiveRequired { what: what.into() })
    }
}

/// Proof that the caller is reading in [`Freshness::Live`] mode. Cannot be
/// built from any other mode. Gate-facing fetch methods take this token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LiveRead {
    _private: (),
}

impl LiveRead {
    /// Always [`Freshness::Live`].
    pub fn freshness(self) -> Freshness {
        Freshness::Live
    }
}

impl From<LiveRead> for Freshness {
    fn from(_: LiveRead) -> Self {
        Freshness::Live
    }
}

/// Data confirmed by a network response at `as_of`. `not_modified` is true
/// when the confirmation was a 304 (cached body revalidated live, §2).
#[derive(Clone, Debug, PartialEq)]
pub struct Live<T> {
    value: T,
    as_of: DateTime<Utc>,
    not_modified: bool,
}

impl<T> Live<T> {
    /// Crate-private: only the gateway (and services built on it) may vouch for liveness.
    #[allow(dead_code)] // used by step-2+ gateway code
    pub(crate) fn new(value: T, as_of: DateTime<Utc>, not_modified: bool) -> Self {
        Self {
            value,
            as_of,
            not_modified,
        }
    }

    pub fn get(&self) -> &T {
        &self.value
    }

    pub fn into_inner(self) -> T {
        self.value
    }

    pub fn as_of(&self) -> DateTime<Utc> {
        self.as_of
    }

    pub fn not_modified(&self) -> bool {
        self.not_modified
    }

    /// Derived data stays live.
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Live<U> {
        Live {
            value: f(self.value),
            as_of: self.as_of,
            not_modified: self.not_modified,
        }
    }
}

/// Where a value came from, for `meta.requests` / `meta.served_304` accounting.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// Full network response (2xx).
    Network,
    /// Conditional request answered 304.
    NotModified,
    /// Served from the local store without a request.
    Cache,
}
