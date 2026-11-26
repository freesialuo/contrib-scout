//! `tracked_repos`: the scope refreshed by `sync --all-tracked` (§9). Step 5 owns this file.
//!
//! These supersede the `track` / `untrack` / `tracked` stubs in `repos.rs`
//! (different names so both files compile side by side).

use super::{from_unix, Store};
use crate::error::Result;
use crate::models::{AgentId, RepoName};
use anyhow::Context;
use chrono::{DateTime, Utc};
use rusqlite::params;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackedRepo {
    pub repo: RepoName,
    #[serde(default)]
    pub added_by: Option<AgentId>,
    pub added_at: DateTime<Utc>,
}

impl Store {
    /// Adds to the scheduled-sync set; returns false if already tracked.
    pub fn add_tracked(
        &mut self,
        repo: &RepoName,
        by: Option<&AgentId>,
        at: DateTime<Utc>,
    ) -> Result<bool> {
        let inserted = self.conn.execute(
            "INSERT INTO tracked_repos (repo, added_by, added_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(repo) DO NOTHING",
            params![repo.to_string(), by.map(AgentId::as_str), at.timestamp()],
        )?;
        Ok(inserted > 0)
    }

    /// Returns false if it was not tracked.
    pub fn remove_tracked(&mut self, repo: &RepoName) -> Result<bool> {
        let removed = self.conn.execute(
            "DELETE FROM tracked_repos WHERE repo = ?1",
            params![repo.to_string()],
        )?;
        Ok(removed > 0)
    }

    /// Every tracked repository, by name.
    pub fn tracked_repos(&self) -> Result<Vec<TrackedRepo>> {
        let mut stmt = self.conn.prepare(
            "SELECT repo, added_by, added_at FROM tracked_repos ORDER BY repo COLLATE NOCASE",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|(repo, by, at)| {
                Ok(TrackedRepo {
                    repo: repo
                        .parse()
                        .with_context(|| format!("stored tracked repo {repo:?}"))?,
                    added_by: by
                        .map(|by| by.parse())
                        .transpose()
                        .with_context(|| format!("stored tracked repo {repo:?} added_by"))?,
                    added_at: from_unix(at)?,
                })
            })
            .collect()
    }
}
