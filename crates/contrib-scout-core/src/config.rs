//! TOML configuration and platform directories (§5, §7, §9).
//!
//! Keys are flat so `config get|set <key>` maps one-to-one. Durations are
//! strings like `30m`, `1h`, `7d` (see [`parse_duration`]). Defaults follow
//! the design's TTL table (§7), mark TTLs (§5), and quota reserves (§9).

use crate::coordinator::Resource;
use crate::error::ScoutError;
use crate::models::Stance;
use anyhow::{anyhow, bail, ensure, Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
    time::Duration,
};

/// Overrides the database path (also `--db`).
pub const ENV_DB: &str = "CONTRIB_SCOUT_DB";
/// Overrides the config path (also `--config`).
pub const ENV_CONFIG: &str = "CONTRIB_SCOUT_CONFIG";
/// Default agent id (also `--agent`).
pub const ENV_AGENT: &str = "CONTRIB_SCOUT_AGENT";

/// Which [`crate::transport::Transport`] to use (§11.1).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportKind {
    /// `gh api -i` subprocess (default; auth and proxy handled by gh).
    #[default]
    Gh,
    /// reqwest with `gh auth token`.
    Http,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub transport: TransportKind,
    /// `gh` executable used by the gh transport and for `gh auth token`.
    pub gh_path: String,
    /// Per-invocation request cap (overridden by `--max-requests`).
    pub max_requests: u32,
    /// Per-process token bucket for api.github.com.
    pub api_rate: f64,
    pub api_burst: u32,
    /// How long single-flight waits for another process's fetch (§9).
    pub lock_wait: String,
    /// Claim lease (§9).
    pub claim_lease: String,

    // Policy mark TTLs (§5). Expiry only sets `review_due`.
    pub mark_ttl_bans: String,
    pub mark_ttl_restricts: String,
    pub mark_ttl_disclose: String,
    pub mark_ttl_allows: String,
    pub mark_ttl_welcomes: String,
    pub mark_ttl_none_found: String,

    // Data TTLs for `Freshness::Default` (§7). Gates ignore these.
    pub policy_files_ttl: String,
    pub repo_ttl: String,
    pub activity_ttl: String,
    pub issue_index_ttl: String,
    pub issue_detail_ttl: String,
    pub search_ttl: String,
    /// Overlap subtracted from the `since` watermark on incremental sync.
    pub index_overlap: String,

    // Shared quota reserves (§9): refuse when remaining <= reserve.
    pub reserve_core: u32,
    pub reserve_search: u32,
    pub reserve_graphql: u32,

    // Global windows across all agents, from `request_log` (§9).
    pub window_per_minute: u32,
    pub window_per_hour: u32,
    pub window_search_per_minute: u32,

    /// Policy file text beyond this many bytes is truncated (with a flag).
    pub policy_text_max_bytes: u32,
    /// Recent comments returned raw by gates and `issue show`.
    pub gate_comments: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            transport: TransportKind::Gh,
            gh_path: "gh".into(),
            max_requests: 50,
            api_rate: 2.0,
            api_burst: 4,
            lock_wait: "10s".into(),
            claim_lease: "48h".into(),
            mark_ttl_bans: "90d".into(),
            mark_ttl_restricts: "30d".into(),
            mark_ttl_disclose: "30d".into(),
            mark_ttl_allows: "14d".into(),
            mark_ttl_welcomes: "14d".into(),
            mark_ttl_none_found: "14d".into(),
            policy_files_ttl: "7d".into(),
            repo_ttl: "7d".into(),
            activity_ttl: "7d".into(),
            issue_index_ttl: "1h".into(),
            issue_detail_ttl: "30m".into(),
            search_ttl: "24h".into(),
            index_overlap: "10m".into(),
            reserve_core: 500,
            reserve_search: 5,
            reserve_graphql: 500,
            window_per_minute: 60,
            window_per_hour: 1500,
            window_search_per_minute: 20,
            policy_text_max_bytes: 64 * 1024,
            gate_comments: 20,
        }
    }
}

/// Resolved data TTLs (§7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ttls {
    pub policy_files: Duration,
    pub repo: Duration,
    pub activity: Duration,
    pub issue_index: Duration,
    pub issue_detail: Duration,
    pub search: Duration,
    pub index_overlap: Duration,
}

/// Shared quota reserve thresholds (§9).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct QuotaReserves {
    pub core: u32,
    pub search: u32,
    pub graphql: u32,
}

impl QuotaReserves {
    pub fn get(&self, resource: Resource) -> u32 {
        match resource {
            Resource::Core => self.core,
            Resource::Search => self.search,
            Resource::Graphql => self.graphql,
        }
    }
}

/// All-agent request windows (§9).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct WindowLimits {
    pub per_minute: u32,
    pub per_hour: u32,
    pub search_per_minute: u32,
}

impl Config {
    fn durations(&self) -> [(&'static str, &String); 15] {
        [
            ("lock_wait", &self.lock_wait),
            ("claim_lease", &self.claim_lease),
            ("mark_ttl_bans", &self.mark_ttl_bans),
            ("mark_ttl_restricts", &self.mark_ttl_restricts),
            ("mark_ttl_disclose", &self.mark_ttl_disclose),
            ("mark_ttl_allows", &self.mark_ttl_allows),
            ("mark_ttl_welcomes", &self.mark_ttl_welcomes),
            ("mark_ttl_none_found", &self.mark_ttl_none_found),
            ("policy_files_ttl", &self.policy_files_ttl),
            ("repo_ttl", &self.repo_ttl),
            ("activity_ttl", &self.activity_ttl),
            ("issue_index_ttl", &self.issue_index_ttl),
            ("issue_detail_ttl", &self.issue_detail_ttl),
            ("search_ttl", &self.search_ttl),
            ("index_overlap", &self.index_overlap),
        ]
    }

    pub fn validate(&self) -> Result<()> {
        for (key, value) in self.durations() {
            parse_duration(value).with_context(|| format!("invalid {key}"))?;
        }
        // A zero lease expires every claim the moment it is taken. (Zero
        // mark and cache TTLs are allowed: they force a re-check.)
        ensure!(
            !parse_duration(&self.claim_lease)?.is_zero(),
            "claim_lease must be longer than zero"
        );
        ensure!(self.max_requests > 0, "max_requests must be at least 1");
        ensure!(
            self.api_rate.is_finite()
                && self.api_rate > 0.0
                && Duration::try_from_secs_f64(self.api_rate.recip()).is_ok(),
            "api_rate must be positive and finite"
        );
        ensure!(self.api_burst > 0, "api_burst must be at least 1");
        ensure!(!self.gh_path.trim().is_empty(), "gh_path must not be empty");
        ensure!(
            self.window_per_minute > 0 && self.window_per_hour > 0,
            "window limits must be at least 1"
        );
        ensure!(
            self.window_search_per_minute > 0,
            "window_search_per_minute must be at least 1"
        );
        ensure!(
            self.policy_text_max_bytes > 0,
            "policy_text_max_bytes must be at least 1"
        );
        Ok(())
    }

    fn duration(value: &str) -> Duration {
        // `validate` runs on every load and set, so this cannot fail for a loaded config.
        parse_duration(value).unwrap_or_default()
    }

    pub fn ttls(&self) -> Ttls {
        Ttls {
            policy_files: Self::duration(&self.policy_files_ttl),
            repo: Self::duration(&self.repo_ttl),
            activity: Self::duration(&self.activity_ttl),
            issue_index: Self::duration(&self.issue_index_ttl),
            issue_detail: Self::duration(&self.issue_detail_ttl),
            search: Self::duration(&self.search_ttl),
            index_overlap: Self::duration(&self.index_overlap),
        }
    }

    /// TTL of a mark with this stance (§5). Unknown stances get the shortest TTL.
    pub fn mark_ttl(&self, stance: &Stance) -> Duration {
        Self::duration(match stance {
            Stance::Bans => &self.mark_ttl_bans,
            Stance::Restricts => &self.mark_ttl_restricts,
            Stance::Disclose => &self.mark_ttl_disclose,
            Stance::Allows => &self.mark_ttl_allows,
            Stance::Welcomes => &self.mark_ttl_welcomes,
            Stance::NoneFound | Stance::Unknown(_) => &self.mark_ttl_none_found,
        })
    }

    pub fn claim_lease(&self) -> Duration {
        Self::duration(&self.claim_lease)
    }

    pub fn lock_wait(&self) -> Duration {
        Self::duration(&self.lock_wait)
    }

    pub fn reserves(&self) -> QuotaReserves {
        QuotaReserves {
            core: self.reserve_core,
            search: self.reserve_search,
            graphql: self.reserve_graphql,
        }
    }

    pub fn windows(&self) -> WindowLimits {
        WindowLimits {
            per_minute: self.window_per_minute,
            per_hour: self.window_per_hour,
            search_per_minute: self.window_search_per_minute,
        }
    }

    /// Every key accepted by `config get|set`.
    pub fn keys() -> Vec<String> {
        toml::Table::try_from(Self::default())
            .map(|table| table.keys().cloned().collect())
            .unwrap_or_default()
    }

    pub fn get(&self, key: &str) -> Result<toml::Value> {
        let values = toml::Table::try_from(self)?;
        values.get(key).cloned().ok_or_else(|| unknown_key(key))
    }

    /// Sets one key from its textual form. Strings take the text verbatim;
    /// other types parse it as a TOML value. Invalid input is a usage error.
    pub fn set(&mut self, key: &str, value: &str) -> Result<()> {
        let mut table = toml::Table::try_from(&*self)?;
        let current = table.get(key).ok_or_else(|| unknown_key(key))?;
        let replacement = if current.is_str() {
            toml::Value::String(value.to_owned())
        } else {
            let mut parsed: toml::Table = toml::from_str(&format!("value = {value}"))
                .map_err(|e| usage(format!("invalid TOML value for {key}: {e}")))?;
            parsed
                .remove("value")
                .ok_or_else(|| usage(format!("expected a single TOML value for {key}")))?
        };
        table.insert(key.to_owned(), replacement);
        let next: Self = toml::Value::Table(table)
            .try_into()
            .map_err(|e| usage(format!("invalid value for {key}: {e}")))?;
        next.validate().map_err(|e| usage(format!("{e:#}")))?;
        *self = next;
        Ok(())
    }
}

fn usage(message: String) -> anyhow::Error {
    ScoutError::Usage(message).into()
}

fn unknown_key(key: &str) -> anyhow::Error {
    usage(format!(
        "unknown config key {key:?}; expected one of {}",
        Config::keys().join(", ")
    ))
}

/// Parses `N` followed by `s`, `m`, `h`, `d`, or `w`.
pub fn parse_duration(value: &str) -> Result<Duration> {
    let Some((quantity, units)) = value.split_at_checked(value.len().saturating_sub(1)) else {
        bail!("invalid duration {value:?}; expected a number followed by s, m, h, d, or w");
    };
    let multiplier = match units {
        "s" => 1,
        "m" => 60,
        "h" => 60 * 60,
        "d" => 24 * 60 * 60,
        "w" => 7 * 24 * 60 * 60,
        _ => bail!("invalid duration {value:?}; expected a number followed by s, m, h, d, or w"),
    };
    // `u64::from_str` accepts a leading `+`; a duration is digits only.
    let digits = !quantity.is_empty() && quantity.bytes().all(|b| b.is_ascii_digit());
    let seconds = digits
        .then(|| quantity.parse::<u64>().ok())
        .flatten()
        .and_then(|quantity| quantity.checked_mul(multiplier))
        .and_then(|seconds| chrono::TimeDelta::try_seconds(seconds.try_into().ok()?))
        .ok_or_else(|| anyhow!("invalid duration {value:?}"))?;
    Ok(Duration::from_secs(seconds.num_seconds().try_into()?))
}

pub fn default_config_path() -> Result<PathBuf> {
    Ok(project_dirs()?.config_dir().join("config.toml"))
}

/// One database shared by every agent on the machine (§6).
pub fn default_db_path() -> Result<PathBuf> {
    Ok(project_dirs()?.data_dir().join("contrib-scout.db"))
}

fn project_dirs() -> Result<ProjectDirs> {
    ProjectDirs::from("", "", "contrib-scout")
        .ok_or_else(|| anyhow!("cannot locate the user data directory"))
}

pub struct ConfigFile {
    path: PathBuf,
}

impl ConfigFile {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Loads and validates; a missing file yields defaults.
    pub fn load(&self) -> Result<Config> {
        let config = self.read_config()?;
        config
            .validate()
            .with_context(|| format!("validate {}", self.path.display()))?;
        Ok(config)
    }

    fn read_config(&self) -> Result<Config> {
        let contents = match fs::read_to_string(&self.path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Config::default()),
            Err(error) => {
                return Err(error).with_context(|| format!("read {}", self.path.display()))
            }
        };
        toml::from_str(&contents).with_context(|| format!("parse {}", self.path.display()))
    }

    /// Reloads under an exclusive lock, applies `edit`, validates, and writes
    /// atomically, so concurrent agents do not lose each other's edits.
    pub fn update<T>(&self, edit: impl FnOnce(&mut Config) -> Result<T>) -> Result<T> {
        let parent = self
            .path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        let lock_path = sibling_with_suffix(&self.path, ".lock")?;
        let lock = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("open {}", lock_path.display()))?;
        lock.lock()
            .with_context(|| format!("lock {}", lock_path.display()))?;

        let mut config = self.read_config()?;
        let output = edit(&mut config)?;
        config.validate()?;
        let encoded = toml::to_string_pretty(&config)?;
        let staging = sibling_with_suffix(&self.path, ".next")?;
        {
            let mut file =
                File::create(&staging).with_context(|| format!("open {}", staging.display()))?;
            file.write_all(encoded.as_bytes())
                .with_context(|| format!("write {}", staging.display()))?;
            file.sync_all()
                .with_context(|| format!("sync {}", staging.display()))?;
        }
        fs::rename(&staging, &self.path)
            .with_context(|| format!("replace {}", self.path.display()))?;
        Ok(output)
    }
}

fn sibling_with_suffix(path: &Path, suffix: &str) -> Result<PathBuf> {
    let mut name = path
        .file_name()
        .ok_or_else(|| anyhow!("invalid config path {}", path.display()))?
        .to_os_string();
    name.push(suffix);
    Ok(path.with_file_name(name))
}
