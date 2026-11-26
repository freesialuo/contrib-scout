//! The single SQLite database every agent shares (§6, §9).
//!
//! Adapted from dwave `wave-core/src/store.rs`: WAL, `busy_timeout`,
//! forward-only migrations in one IMMEDIATE transaction, and `fs2` file locks.
//! Domain methods live in one file per domain (`policy.rs`, `repos.rs`,
//! `issues.rs`, `claims.rs`, `quota.rs`, `cache.rs`) as `impl Store` blocks.

pub mod cache;
pub mod claims;
pub mod issues;
pub mod policy;
pub mod quota;
pub mod repos;
pub mod schema;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use fs2::FileExt;
use rusqlite::{params, Connection, TransactionBehavior};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

pub use cache::{CacheStats, CachedResponse, DiscoverRun, PruneStats};
pub use issues::{IssueFilter, SyncState};
pub use policy::{CachedPolicyFile, POLICY_SNAPSHOT_PATH};

/// Services share one connection per process.
pub type SharedStore = Arc<Mutex<Store>>;

pub struct Store {
    pub(crate) conn: Connection,
    path: Option<PathBuf>,
}

/// A cached value with the time it was fetched.
#[derive(Clone, Debug, PartialEq)]
pub struct Cached<T> {
    pub data: T,
    pub fetched_at: DateTime<Utc>,
}

/// Held for the duration of a scoped critical section; released on drop.
pub struct ScopeLock {
    _file: Option<File>,
}

impl Store {
    /// Opens (creating if needed) and migrates. `:memory:` opens an in-memory store.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if path == Path::new(":memory:") {
            return Self::memory();
        }
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create database directory {}", parent.display()))?;
        }
        let conn =
            Connection::open(path).with_context(|| format!("open database {}", path.display()))?;
        let path = path.canonicalize().context("resolve database path")?;
        Self::initialize(conn, Some(path))
    }

    pub fn memory() -> Result<Self> {
        Self::initialize(Connection::open_in_memory()?, None)
    }

    pub fn shared(self) -> SharedStore {
        Arc::new(Mutex::new(self))
    }

    /// The canonical database path; `None` for in-memory stores.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    fn initialize(mut conn: Connection, path: Option<PathBuf>) -> Result<Self> {
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "foreign_keys", true)?;
        if path.is_some() {
            conn.pragma_update(None, "journal_mode", "WAL")?;
        }
        let migrations = schema::MIGRATIONS;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let version: u32 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
        let version = version as usize;
        if version > migrations.len() {
            bail!(
                "database schema version {version} is newer than supported version {}",
                migrations.len()
            );
        }
        for (index, migration) in migrations.iter().enumerate().skip(version) {
            let next = index + 1;
            tx.execute_batch(migration)
                .with_context(|| format!("migrate database to schema version {next}"))?;
            tx.execute(
                "INSERT INTO meta (key, value) VALUES ('schema_version', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![next.to_string()],
            )?;
            tx.pragma_update(None, "user_version", i64::try_from(next)?)?;
        }
        tx.commit()?;
        Ok(Self { conn, path })
    }

    /// Current schema version (`PRAGMA user_version`).
    pub fn schema_version(&self) -> Result<u32> {
        Ok(self
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))?)
    }

    /// Names of all user tables, sorted.
    pub fn tables(&self) -> Result<Vec<String>> {
        let mut statement = self.conn.prepare(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )?;
        let names = statement
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<String>>>()?;
        Ok(names)
    }

    /// Cross-process exclusive lock on `scope` (a sibling `.lock` file named by
    /// a stable hash). Returns `Ok(None)` if not acquired within `max_wait`.
    /// In-memory stores always succeed. Basis for single-flight (§9).
    pub fn lock_scope(&self, scope: &str, max_wait: Duration) -> Result<Option<ScopeLock>> {
        let Some(path) = &self.path else {
            return Ok(Some(ScopeLock { _file: None }));
        };
        // FNV-1a: stable across processes, and keeps URLs out of file names.
        let hash = scope.bytes().fold(0xcbf29ce484222325u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
        });
        let mut name = path.as_os_str().to_owned();
        name.push(format!(".scope-{hash:016x}.lock"));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&name)
            .with_context(|| format!("open scope lock {}", Path::new(&name).display()))?;
        let start = Instant::now();
        loop {
            match FileExt::try_lock_exclusive(&file) {
                Ok(()) => return Ok(Some(ScopeLock { _file: Some(file) })),
                Err(error)
                    if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() =>
                {
                    if start.elapsed() >= max_wait {
                        return Ok(None);
                    }
                    thread::sleep(
                        Duration::from_millis(25).min(max_wait.saturating_sub(start.elapsed())),
                    );
                }
                Err(error) => return Err(error).context("acquire scope lock"),
            }
        }
    }
}

/// Unix seconds → UTC.
#[allow(dead_code)] // used by the domain files in later steps
pub(crate) fn from_unix(seconds: i64) -> Result<DateTime<Utc>> {
    DateTime::from_timestamp(seconds, 0)
        .ok_or_else(|| anyhow::anyhow!("invalid timestamp {seconds}"))
}
