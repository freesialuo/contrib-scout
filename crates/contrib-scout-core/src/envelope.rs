//! Versioned JSON envelope (§10), shared by `--json`, `--ndjson`, and a future MCP layer.
//!
//! ```json
//! { "schema": "contrib-scout/v1",
//!   "meta": { "as_of", "requests", "served_304", "partial", "warnings", "quota", "reminders" },
//!   "data": ..., "page": {...}?, "error": {...}? }
//! ```
//! Every command that touches a repository fills `meta.reminders` (§5).

use crate::coordinator::QuotaSnapshot;
use crate::error::ScoutError;
use crate::models::{ParseError, Reminder};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub const SCHEMA: &str = "contrib-scout/v1";

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Meta {
    pub as_of: Option<DateTime<Utc>>,
    /// Network requests sent by this invocation (304s included).
    pub requests: u32,
    /// Requests answered `304 Not Modified`.
    pub served_304: u32,
    /// Some data could not be refreshed and came from the cache, or is missing.
    pub partial: bool,
    pub warnings: Vec<String>,
    /// Last observed shared quota per resource.
    pub quota: Vec<QuotaSnapshot>,
    pub reminders: Vec<Reminder>,
}

impl Meta {
    /// Adds a reminder unless an identical one is already present.
    pub fn remind(&mut self, reminder: Reminder) {
        if !self.reminders.contains(&reminder) {
            self.reminders.push(reminder);
        }
    }

    pub fn warn(&mut self, warning: impl Into<String>) {
        let warning = warning.into();
        if !self.warnings.contains(&warning) {
            self.warnings.push(warning);
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Page {
    pub page: u32,
    pub limit: u32,
    pub total: u64,
}

/// Machine-readable error, present only when the command failed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorInfo {
    /// [`ScoutError::code`], `usage` for parse errors, or `error`.
    pub code: String,
    pub message: String,
    pub exit_code: u8,
}

impl ErrorInfo {
    pub fn from_error(error: &anyhow::Error) -> Self {
        let message = format!("{error:#}");
        if let Some(scout) = ScoutError::find(error) {
            return Self {
                code: scout.code().into(),
                message,
                exit_code: scout.exit_code(),
            };
        }
        if error.chain().any(|e| e.is::<ParseError>()) {
            return Self {
                code: "usage".into(),
                message,
                exit_code: 2,
            };
        }
        Self {
            code: "error".into(),
            message,
            exit_code: 1,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Envelope<T> {
    pub schema: String,
    pub meta: Meta,
    pub data: T,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page: Option<Page>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorInfo>,
}

impl<T> Envelope<T> {
    pub fn new(meta: Meta, data: T) -> Self {
        Self {
            schema: SCHEMA.into(),
            meta,
            data,
            page: None,
            error: None,
        }
    }
}

impl Envelope<serde_json::Value> {
    /// `data: null` plus `error`.
    pub fn failure(meta: Meta, error: &anyhow::Error) -> Self {
        Self {
            error: Some(ErrorInfo::from_error(error)),
            ..Self::new(meta, serde_json::Value::Null)
        }
    }
}
