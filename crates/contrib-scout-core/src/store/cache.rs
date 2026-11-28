//! `http_cache`, `discover_runs`, and whole-database stats / prune (§6, §7).
//! Step 2 owns the HTTP cache; step 4 the discover runs.

use super::{from_unix, schema, Cached, Store};
use crate::error::Result;
use crate::models::RepoName;
use chrono::{DateTime, Utc};
use rusqlite::{params, OptionalExtension};
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
    /// The gate audit log, which only `cache prune --gate-log-older-than` prunes.
    pub gate_log: GateLogStats,
}

/// Size of `gate_log`: each row stores a whole gate report as JSON.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateLogStats {
    pub rows: u64,
    /// Bytes of the stored reasons and report snapshots (row overhead excluded).
    pub bytes: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PruneStats {
    /// Rows deleted per table. Policy marks and history are never pruned.
    pub deleted: BTreeMap<String, u64>,
}

/// `(table, DELETE statement)` run by [`Store::prune`]; `?1` is the cutoff.
///
/// Not pruned: `policy_marks`, `policy_mark_history`, `tracked_repos`, `gate_log`
/// (audit trails; `gate_log` only on request, see [`Store::prune_gate_log`]), active claims, and `issues` / `sync_state` (deleting indexed
/// issues behind an incremental `since` watermark would silently lose them).
const PRUNE: &[(&str, &str)] = &[
    ("http_cache", "DELETE FROM http_cache WHERE fetched_at < ?1"),
    ("request_log", "DELETE FROM request_log WHERE at < ?1"),
    (
        "discover_runs",
        "DELETE FROM discover_runs WHERE fetched_at < ?1",
    ),
    // Per repository, all-or-nothing, keyed by the snapshot marker's
    // `fetched_at` (path ''): a marker kept without its file / absent rows
    // would be served as a complete, empty snapshot. Rows of a repository
    // without a marker are pruned by their own `fetched_at`.
    (
        "policy_files",
        "DELETE FROM policy_files WHERE repo IN (
             SELECT repo FROM policy_files WHERE path = '' AND fetched_at < ?1
         ) OR (fetched_at < ?1 AND repo NOT IN (
             SELECT repo FROM policy_files WHERE path = ''
         ))",
    ),
    (
        "repo_activity",
        "DELETE FROM repo_activity WHERE computed_at < ?1",
    ),
    ("repos", "DELETE FROM repos WHERE fetched_at < ?1"),
    (
        "claims",
        "DELETE FROM claims WHERE status <> 'active' AND updated_at < ?1",
    ),
];

impl Store {
    pub fn cached_response(&self, key: &str) -> Result<Option<Cached<CachedResponse>>> {
        let row = self
            .conn
            .query_row(
                "SELECT status, etag, last_modified, headers_json, body, fetched_at
                 FROM http_cache WHERE key = ?1",
                params![key],
                |row| {
                    Ok((
                        row.get::<_, u16>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<Vec<u8>>>(4)?,
                        row.get::<_, i64>(5)?,
                    ))
                },
            )
            .optional()?;
        let Some((status, etag, last_modified, headers, body, fetched_at)) = row else {
            return Ok(None);
        };
        Ok(Some(Cached {
            data: CachedResponse {
                key: key.to_owned(),
                status,
                etag,
                last_modified,
                headers: serde_json::from_str(&headers)?,
                body: body.unwrap_or_default(),
            },
            fetched_at: from_unix(fetched_at)?,
        }))
    }

    pub fn save_response(&mut self, response: &CachedResponse, at: DateTime<Utc>) -> Result<()> {
        self.conn.execute(
            "INSERT INTO http_cache (key, etag, last_modified, status, headers_json, body, fetched_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(key) DO UPDATE SET
                 etag = excluded.etag,
                 last_modified = excluded.last_modified,
                 status = excluded.status,
                 headers_json = excluded.headers_json,
                 body = excluded.body,
                 fetched_at = excluded.fetched_at",
            params![
                response.key,
                response.etag,
                response.last_modified,
                response.status,
                serde_json::to_string(&response.headers)?,
                response.body,
                at.timestamp(),
            ],
        )?;
        Ok(())
    }

    /// Updates `fetched_at` after a 304. Overwrites rather than keeping the
    /// later value: a stored `fetched_at` after `at` (clock skew) would
    /// otherwise never come back to the present.
    pub fn touch_response(&mut self, key: &str, at: DateTime<Utc>) -> Result<()> {
        self.conn.execute(
            "UPDATE http_cache SET fetched_at = ?2 WHERE key = ?1",
            params![key, at.timestamp()],
        )?;
        Ok(())
    }

    pub fn discover_run(&self, query_key: &str) -> Result<Option<Cached<DiscoverRun>>> {
        let row = self
            .conn
            .query_row(
                "SELECT query, params_json, repos_json, fetched_at FROM discover_runs WHERE query_key = ?1",
                params![query_key],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()?;
        let Some((query, params, repos, fetched_at)) = row else {
            return Ok(None);
        };
        Ok(Some(Cached {
            data: DiscoverRun {
                query_key: query_key.to_owned(),
                query,
                params: serde_json::from_str(&params)?,
                repos: serde_json::from_str(&repos)?,
            },
            fetched_at: from_unix(fetched_at)?,
        }))
    }

    pub fn save_discover_run(&mut self, run: &DiscoverRun, at: DateTime<Utc>) -> Result<()> {
        self.conn.execute(
            "INSERT INTO discover_runs (query_key, query, params_json, repos_json, fetched_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(query_key) DO UPDATE SET
                 query = excluded.query,
                 params_json = excluded.params_json,
                 repos_json = excluded.repos_json,
                 fetched_at = excluded.fetched_at",
            params![
                run.query_key,
                run.query,
                serde_json::to_string(&run.params)?,
                serde_json::to_string(&run.repos)?,
                at.timestamp(),
            ],
        )?;
        Ok(())
    }

    pub fn stats(&self) -> Result<CacheStats> {
        let mut rows = BTreeMap::new();
        for table in schema::TABLES {
            // Table names come from the fixed schema list, never from input.
            let count: i64 =
                self.conn
                    .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                        row.get(0)
                    })?;
            rows.insert(
                (*table).to_owned(),
                u64::try_from(count).unwrap_or_default(),
            );
        }
        let bytes = if self.path().is_some() {
            let pages: i64 = self
                .conn
                .pragma_query_value(None, "page_count", |row| row.get(0))?;
            let size: i64 = self
                .conn
                .pragma_query_value(None, "page_size", |row| row.get(0))?;
            u64::try_from(pages.saturating_mul(size)).ok()
        } else {
            None
        };
        let (gate_rows, gate_bytes): (i64, i64) = self.conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(LENGTH(CAST(reasons_json AS BLOB))
                 + COALESCE(LENGTH(CAST(snapshot_json AS BLOB)), 0)), 0)
             FROM gate_log",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let gate_log = GateLogStats {
            rows: u64::try_from(gate_rows).unwrap_or_default(),
            bytes: u64::try_from(gate_bytes).unwrap_or_default(),
        };
        Ok(CacheStats {
            rows,
            bytes,
            gate_log,
        })
    }

    /// Deletes cached data fetched before `older_than`. Never touches
    /// `policy_marks`, `policy_mark_history`, active claims, or `tracked_repos`.
    /// See `PRUNE` for exactly which tables are pruned.
    pub fn prune(&mut self, older_than: DateTime<Utc>) -> Result<PruneStats> {
        let cutoff = older_than.timestamp();
        let tx = self.conn.transaction()?;
        let mut deleted = BTreeMap::new();
        for (table, statement) in PRUNE {
            let count = tx.execute(statement, params![cutoff])?;
            deleted.insert((*table).to_owned(), count as u64);
        }
        tx.commit()?;
        Ok(PruneStats { deleted })
    }

    /// Deletes `gate_log` rows whose gate ran (`as_of`) before `older_than`;
    /// returns how many. [`Store::prune`] never does this: the log is an audit
    /// trail, pruned only when explicitly asked (`--gate-log-older-than`).
    pub fn prune_gate_log(&mut self, older_than: DateTime<Utc>) -> Result<u64> {
        let count = self.conn.execute(
            "DELETE FROM gate_log WHERE as_of < ?1",
            params![older_than.timestamp()],
        )?;
        Ok(count as u64)
    }
}
