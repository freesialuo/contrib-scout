//! `quota`, `request_log` (§6, §9). Step 2 owns this file.
//!
//! `quota` holds the newest `x-ratelimit-*` observation per resource;
//! `request_log` has one row per request any agent sent (or is about to send),
//! which is what the all-agent per-minute / per-hour windows count.

use super::{from_unix, Store};
use crate::config::WindowLimits;
use crate::coordinator::{QuotaSnapshot, Resource};
use crate::error::{Result, ScoutError};
use crate::models::AgentId;
use chrono::{DateTime, TimeDelta, Utc};
use rusqlite::{params, OptionalExtension, TransactionBehavior};

impl Store {
    /// Records the newest `x-ratelimit-*` observation (older ones never overwrite newer).
    ///
    /// "Newer" means a later reset window, or the same window observed at the
    /// same time or later. Within one second the smaller `remaining` wins, since
    /// responses can arrive out of order.
    pub fn record_quota(&mut self, snapshot: &QuotaSnapshot) -> Result<()> {
        self.conn.execute(
            "INSERT INTO quota (resource, limit_total, remaining, reset_at, observed_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(resource) DO UPDATE SET
                 limit_total = excluded.limit_total,
                 remaining = excluded.remaining,
                 reset_at = excluded.reset_at,
                 observed_at = excluded.observed_at
             WHERE excluded.reset_at > quota.reset_at
                OR (excluded.reset_at = quota.reset_at AND (
                        excluded.observed_at > quota.observed_at
                     OR (excluded.observed_at = quota.observed_at
                         AND excluded.remaining <= quota.remaining)))",
            params![
                snapshot.resource.as_str(),
                snapshot.limit,
                snapshot.remaining,
                snapshot.reset_at.timestamp(),
                snapshot.observed_at.timestamp(),
            ],
        )?;
        Ok(())
    }

    pub fn quota(&self, resource: Resource) -> Result<Option<QuotaSnapshot>> {
        let row = self
            .conn
            .query_row(
                "SELECT limit_total, remaining, reset_at, observed_at FROM quota WHERE resource = ?1",
                params![resource.as_str()],
                |row| {
                    Ok((
                        row.get::<_, u32>(0)?,
                        row.get::<_, u32>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()?;
        row.map(|(limit, remaining, reset_at, observed_at)| {
            Ok(QuotaSnapshot {
                resource,
                limit,
                remaining,
                reset_at: from_unix(reset_at)?,
                observed_at: from_unix(observed_at)?,
            })
        })
        .transpose()
    }

    pub fn quotas(&self) -> Result<Vec<QuotaSnapshot>> {
        let mut snapshots = Vec::new();
        for resource in Resource::ALL {
            if let Some(snapshot) = self.quota(resource)? {
                snapshots.push(snapshot);
            }
        }
        Ok(snapshots)
    }

    /// Appends one request to the all-agent window log.
    pub fn log_request(
        &mut self,
        resource: Resource,
        agent: Option<&AgentId>,
        status: Option<u16>,
        at: DateTime<Utc>,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO request_log (at, resource, agent_id, status) VALUES (?1, ?2, ?3, ?4)",
            params![
                at.timestamp(),
                resource.as_str(),
                agent.map(AgentId::as_str),
                status
            ],
        )?;
        Ok(())
    }

    /// Requests logged at or after `since`; `None` counts every resource.
    pub fn requests_since(&self, resource: Option<Resource>, since: DateTime<Utc>) -> Result<u32> {
        let count = match resource {
            Some(resource) => self.conn.query_row(
                "SELECT COUNT(*) FROM request_log WHERE resource = ?1 AND at >= ?2",
                params![resource.as_str(), since.timestamp()],
                |row| row.get(0),
            )?,
            None => self.conn.query_row(
                "SELECT COUNT(*) FROM request_log WHERE at >= ?1",
                params![since.timestamp()],
                |row| row.get(0),
            )?,
        };
        Ok(count)
    }

    /// Deletes rows older than `before`; returns how many.
    pub fn prune_request_log(&mut self, before: DateTime<Utc>) -> Result<usize> {
        Ok(self.conn.execute(
            "DELETE FROM request_log WHERE at < ?1",
            params![before.timestamp()],
        )?)
    }

    /// Atomically checks the shared quota and the all-agent windows, then logs
    /// `n` pending requests (status `NULL`) for `agent`. Returns the new
    /// `request_log` row ids, to be settled with [`Store::settle_requests`].
    ///
    /// Runs in one IMMEDIATE transaction, so two processes cannot both take the
    /// last slot of a window. Refusals:
    /// - [`ScoutError::QuotaReserved`] when the last observed `remaining` is at or
    ///   below `reserve` and its `reset_at` is still in the future (a snapshot
    ///   whose reset has passed is stale and ignored);
    /// - [`ScoutError::WindowLimit`] when `n` more requests would exceed the
    ///   per-minute or per-hour limit over all resources, or the search
    ///   per-minute limit for [`Resource::Search`].
    pub fn reserve_requests(
        &mut self,
        resource: Resource,
        n: u32,
        agent: Option<&AgentId>,
        reserve: u32,
        windows: WindowLimits,
        now: DateTime<Utc>,
    ) -> Result<Vec<i64>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        // Re-read inside the write lock so the check and the insert are atomic.
        let fresh: Option<(u32, i64)> = tx
            .query_row(
                "SELECT remaining, reset_at FROM quota WHERE resource = ?1",
                params![resource.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((remaining, reset_at)) = fresh {
            if remaining <= reserve && reset_at > now.timestamp() {
                return Err(ScoutError::QuotaReserved {
                    resource,
                    remaining,
                    reserve,
                    reset_at: DateTime::from_timestamp(reset_at, 0),
                }
                .into());
            }
        }

        let count = |since: DateTime<Utc>, only: Option<Resource>| -> rusqlite::Result<u32> {
            match only {
                Some(resource) => tx.query_row(
                    "SELECT COUNT(*) FROM request_log WHERE resource = ?1 AND at > ?2",
                    params![resource.as_str(), since.timestamp()],
                    |row| row.get(0),
                ),
                None => tx.query_row(
                    "SELECT COUNT(*) FROM request_log WHERE at > ?1",
                    params![since.timestamp()],
                    |row| row.get(0),
                ),
            }
        };
        let minute_ago = now - TimeDelta::minutes(1);
        let hour_ago = now - TimeDelta::hours(1);
        let mut checks = vec![
            ("minute", windows.per_minute, count(minute_ago, None)?),
            ("hour", windows.per_hour, count(hour_ago, None)?),
        ];
        if resource == Resource::Search {
            checks.push((
                "minute",
                windows.search_per_minute,
                count(minute_ago, Some(Resource::Search))?,
            ));
        }
        for (window, limit, used) in checks {
            if used.saturating_add(n) > limit {
                return Err(ScoutError::WindowLimit {
                    resource,
                    window,
                    limit,
                }
                .into());
            }
        }

        let mut ids = Vec::with_capacity(n as usize);
        for _ in 0..n {
            tx.execute(
                "INSERT INTO request_log (at, resource, agent_id, status) VALUES (?1, ?2, ?3, NULL)",
                params![now.timestamp(), resource.as_str(), agent.map(AgentId::as_str)],
            )?;
            ids.push(tx.last_insert_rowid());
        }
        tx.commit()?;
        Ok(ids)
    }

    /// Fills in the HTTP status of requests logged by [`Store::reserve_requests`]
    /// (`None` when no response was obtained).
    pub fn settle_requests(&mut self, ids: &[i64], status: Option<u16>) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let tx = self.conn.transaction()?;
        for id in ids {
            tx.execute(
                "UPDATE request_log SET status = ?2 WHERE id = ?1",
                params![id, status],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
}
