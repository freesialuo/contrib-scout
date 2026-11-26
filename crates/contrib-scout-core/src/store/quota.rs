//! `quota`, `request_log` (§6, §9). Step 2 owns this file.

use super::Store;
use crate::coordinator::{QuotaSnapshot, Resource};
use crate::error::{not_implemented, Result};
use crate::models::AgentId;
use chrono::{DateTime, Utc};

impl Store {
    /// Records the newest `x-ratelimit-*` observation (older ones never overwrite newer).
    pub fn record_quota(&mut self, _snapshot: &QuotaSnapshot) -> Result<()> {
        Err(not_implemented("quota storage"))
    }

    pub fn quota(&self, _resource: Resource) -> Result<Option<QuotaSnapshot>> {
        Err(not_implemented("quota storage"))
    }

    pub fn quotas(&self) -> Result<Vec<QuotaSnapshot>> {
        Err(not_implemented("quota storage"))
    }

    /// Appends one request to the all-agent window log.
    pub fn log_request(
        &mut self,
        _resource: Resource,
        _agent: Option<&AgentId>,
        _status: Option<u16>,
        _at: DateTime<Utc>,
    ) -> Result<()> {
        Err(not_implemented("request log"))
    }

    /// Requests logged at or after `since`; `None` counts every resource.
    pub fn requests_since(
        &self,
        _resource: Option<Resource>,
        _since: DateTime<Utc>,
    ) -> Result<u32> {
        Err(not_implemented("request log"))
    }

    /// Deletes rows older than `before`; returns how many.
    pub fn prune_request_log(&mut self, _before: DateTime<Utc>) -> Result<usize> {
        Err(not_implemented("request log"))
    }
}
