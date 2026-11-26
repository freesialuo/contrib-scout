//! Typed errors that callers must be able to tell apart.
//!
//! Functions return [`anyhow::Result`]; a [`ScoutError`] anywhere in the error
//! chain decides the machine-readable `error.code` and the exit code (§10:
//! `1` error, `2` usage). Use [`ScoutError::find`] to look it up.

use crate::coordinator::Resource;
use chrono::{DateTime, Utc};
use std::fmt;

pub type Result<T, E = anyhow::Error> = std::result::Result<T, E>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScoutError {
    /// A command or service that a later delivery step will implement (§12).
    NotImplemented { feature: &'static str },
    /// Invalid arguments or values supplied by the caller. Exit code 2.
    Usage(String),
    /// The per-invocation request budget (`--max-requests`) is spent.
    BudgetExhausted { limit: u32 },
    /// Shared quota is at or below its reserve threshold (§9, default core 500, search 5).
    QuotaReserved {
        resource: Resource,
        remaining: u32,
        reserve: u32,
        reset_at: Option<DateTime<Utc>>,
    },
    /// The global, all-agent request window (per minute / per hour) is full (§9).
    WindowLimit {
        resource: Resource,
        window: &'static str,
        limit: u32,
    },
    /// A live read was required but the mode forbids the network (e.g. `--offline` on a gate, §4).
    LiveRequired { what: String },
    /// The network or `gh` could not be reached. Gates fail closed on this (§8).
    NetworkUnavailable { message: String },
    /// GitHub answered with a non-retryable status.
    HttpStatus { status: u16, url: String },
    /// A requested local record does not exist.
    NotFound { what: String },
}

impl ScoutError {
    /// Stable snake_case identifier written to `error.code` in JSON output.
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotImplemented { .. } => "not_implemented",
            Self::Usage(_) => "usage",
            Self::BudgetExhausted { .. } => "budget_exhausted",
            Self::QuotaReserved { .. } => "quota_reserved",
            Self::WindowLimit { .. } => "window_limit",
            Self::LiveRequired { .. } => "live_required",
            Self::NetworkUnavailable { .. } => "network_unavailable",
            Self::HttpStatus { .. } => "http_status",
            Self::NotFound { .. } => "not_found",
        }
    }

    /// Process exit code (§10): `2` for usage errors, `1` for everything else.
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Usage(_) => 2,
            _ => 1,
        }
    }

    /// The first `ScoutError` in an error chain, if any.
    pub fn find(error: &anyhow::Error) -> Option<&ScoutError> {
        error.chain().find_map(|e| e.downcast_ref::<ScoutError>())
    }
}

impl fmt::Display for ScoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotImplemented { feature } => write!(f, "{feature} is not implemented yet"),
            Self::Usage(message) => f.write_str(message),
            Self::BudgetExhausted { limit } => {
                write!(f, "request budget exhausted ({limit} requests)")
            }
            Self::QuotaReserved {
                resource,
                remaining,
                reserve,
                reset_at,
            } => {
                write!(
                    f,
                    "{resource} quota has {remaining} requests left, at or below the reserve of {reserve}"
                )?;
                if let Some(reset) = reset_at {
                    write!(f, "; resets at {}", reset.to_rfc3339())?;
                }
                Ok(())
            }
            Self::WindowLimit {
                resource,
                window,
                limit,
            } => write!(
                f,
                "shared {resource} window limit reached ({limit} requests per {window})"
            ),
            Self::LiveRequired { what } => {
                write!(f, "{what} requires live GitHub data; remove --offline")
            }
            Self::NetworkUnavailable { message } => write!(f, "network unavailable: {message}"),
            Self::HttpStatus { status, url } => write!(f, "{url} returned HTTP {status}"),
            Self::NotFound { what } => write!(f, "{what} not found"),
        }
    }
}

impl std::error::Error for ScoutError {}

/// Shorthand for the error every step-1 stub returns.
pub fn not_implemented(feature: &'static str) -> anyhow::Error {
    ScoutError::NotImplemented { feature }.into()
}
