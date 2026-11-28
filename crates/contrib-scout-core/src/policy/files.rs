//! Candidate policy file fetching (§2, §5 step 4, §8). Raw text only.
//!
//! Request plan per call, for the sources `{owner}/.github` (org) and the
//! repository itself:
//!
//! 1. One conditional REST `GET /repos/{source}/commits/HEAD` with the
//!    `application/vnd.github.sha` media type per source. This is the only
//!    mutable read: a 304 confirms that every file cached for that commit is
//!    still current, so a warm check costs one 304 per source. A 404 (or 409,
//!    empty repository) means the source has no files.
//! 2. Only for sources whose HEAD moved (or that were never listed): one
//!    GraphQL query for all of them together, listing the root, `.github/`
//!    and `docs/` trees (and their `PULL_REQUEST_TEMPLATE/` directories) at
//!    that commit and returning the text of every exact-case candidate path
//!    in the same response.
//! 3. Only when a match differs from [`CANDIDATE_PATHS`] (another case or
//!    extension, a template-directory file) or a text is missing from the
//!    cache: one more GraphQL query fetching those blobs. Never offline.
//!
//! Cold: 2 REST + 1 (rarely 2) GraphQL requests. Warm within `policy_files_ttl`
//! (`Default`): zero requests. Warm after the TTL: one 304 per source.
//!
//! GraphQL has no ETags, which is why freshness and liveness are decided by
//! the REST commit check (through [`Gateway::get`] / [`Gateway::get_live`]);
//! everything else is addressed by commit SHA, hence immutable. In live mode
//! a moved HEAD triggers the GraphQL reads, which are always network reads.
//!
//! Results are cached in `policy_files`: one snapshot marker per source
//! ([`POLICY_SNAPSHOT_PATH`]), one row per found file (text truncated to
//! `policy_text_max_bytes`, full `size` kept), and one row per absent
//! candidate path, so `NoneFound` evidence can cite what was checked.

use super::PolicyFiles;
use crate::config::Config;
use crate::error::{Result, ScoutError};
use crate::freshness::{Freshness, Live, LiveRead, Origin};
use crate::gateway::Gateway;
use crate::models::{PolicyFile, RepoName};
use crate::store::{Cached, CachedPolicyFile, SharedStore, Store, POLICY_SNAPSHOT_PATH};
use crate::transport::{ApiRequest, ApiResponse};
use anyhow::{anyhow, bail, Context};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::sync::{Arc, MutexGuard};

/// Where AI policies are found (§2). Checked in both `{owner}/.github` and the
/// repository. Matched case-insensitively against listings of the root,
/// `.github/`, and `docs/`; see [`CANDIDATE_STEMS`] and [`TEMPLATE_DIRS`]
/// for the other spellings a candidate stands for.
pub const CANDIDATE_PATHS: &[&str] = &[
    "AI_POLICY.md",
    "LLM_POLICY.md",
    ".github/AI_POLICY.md",
    ".github/LLM_POLICY.md",
    "docs/AI_POLICY.md",
    "docs/LLM_POLICY.md",
    "CONTRIBUTING.md",
    ".github/CONTRIBUTING.md",
    "docs/CONTRIBUTING.md",
    "AGENTS.md",
    "CLAUDE.md",
    ".github/copilot-instructions.md",
    ".github/PULL_REQUEST_TEMPLATE.md",
    ".github/pull_request_template.md",
    "PULL_REQUEST_TEMPLATE.md",
    "docs/pull_request_template.md",
    "README.md",
];

/// Directories listed to find candidates: root, `.github`, `docs`.
pub const PROBE_DIRS: &[&str] = &["", ".github", "docs"];

/// File-name stems (the name up to its first `.`, any case, `-` read as `_`)
/// matched in every [`PROBE_DIRS`] entry with any extension or none:
/// `CONTRIBUTING.rst`, `docs/ai_policy.txt`, `.github/CONTRIBUTING`. Each
/// stem has a [`CANDIDATE_PATHS`] entry in every probed directory.
pub const CANDIDATE_STEMS: &[&str] = &["ai_policy", "llm_policy", "contributing"];

/// Spellings of the multi-template directory listed in every [`PROBE_DIRS`]
/// entry (`.github/PULL_REQUEST_TEMPLATE/*.md`); each file in it stands for
/// that directory's single-file PR template candidate. Git paths are
/// case-sensitive, so each spelling is its own lookup in the listing query.
pub const TEMPLATE_DIRS: &[&str] = &["PULL_REQUEST_TEMPLATE", "pull_request_template"];

/// Git file mode of a symbolic link: the blob's text is the link target.
const SYMLINK_MODE: u64 = 0o120000;

/// Kept in a found row's `etag` (unused for files: blobs are addressed by
/// oid) so the symlink flag survives the cache.
const SYMLINK_TAG: &str = "git-mode:120000";

/// Media type that makes `commits/{ref}` return the bare SHA.
pub const SHA_MEDIA_TYPE: &str = "application/vnd.github.sha";

/// `(repository, path)` pairs to probe: org-level first, then repo-level, so
/// a repository policy can override the org's (§2). Skips the org pass when
/// `repo` is itself `{owner}/.github`.
pub fn candidate_locations(repo: &RepoName) -> Vec<(RepoName, &'static str)> {
    policy_sources(repo)
        .into_iter()
        .flat_map(|source| {
            CANDIDATE_PATHS
                .iter()
                .map(move |path| (source.clone(), *path))
        })
        .collect()
}

/// The repositories probed for `repo`: `{owner}/.github` (unless `repo` is
/// that repository), then `repo`.
pub fn policy_sources(repo: &RepoName) -> Vec<RepoName> {
    let org = repo.org_policy_repo();
    let mut sources = Vec::new();
    if !org.name().eq_ignore_ascii_case(repo.name()) {
        sources.push(org);
    }
    sources.push(repo.clone());
    sources
}

/// Commit-pinned permalink of `path` in `source`.
pub fn pinned_url(source: &RepoName, commit: &str, path: &str) -> String {
    format!("https://github.com/{source}/blob/{commit}/{path}")
}

/// Truncates to at most `max` bytes on a character boundary.
pub fn truncate_text(text: &str, max: usize) -> (String, bool) {
    if text.len() <= max {
        return (text.to_owned(), false);
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_owned(), true)
}

/// [`CANDIDATE_PATHS`] without case-insensitive duplicates (first spelling wins).
pub fn unique_candidates() -> impl Iterator<Item = &'static str> {
    CANDIDATE_PATHS.iter().enumerate().filter_map(|(i, path)| {
        let first = CANDIDATE_PATHS[..i]
            .iter()
            .all(|earlier| !earlier.eq_ignore_ascii_case(path));
        first.then_some(*path)
    })
}

/// `(directory, file name)` of a repository path.
fn split_path(path: &str) -> (&str, &str) {
    path.rsplit_once('/').unwrap_or(("", path))
}

/// The [`CANDIDATE_STEMS`] form of a file name.
fn file_stem(name: &str) -> String {
    name.split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
        .replace('-', "_")
}

/// Index of the candidate `path` stands for: the same path in any case; a
/// file with one of the [`CANDIDATE_STEMS`] in a probed directory; or a file
/// in a [`TEMPLATE_DIRS`] directory, standing for that directory's
/// single-file template.
fn candidate_rank(path: &str) -> Option<usize> {
    let exact = CANDIDATE_PATHS
        .iter()
        .position(|candidate| candidate.eq_ignore_ascii_case(path));
    if exact.is_some() {
        return exact;
    }
    let (dir, name) = split_path(path);
    let (parent, sub) = split_path(dir);
    let (dir, stem) = if !dir.is_empty() && TEMPLATE_DIRS[0].eq_ignore_ascii_case(sub) {
        (parent, TEMPLATE_DIRS[1].to_owned())
    } else {
        let stem = file_stem(name);
        if !CANDIDATE_STEMS.contains(&stem.as_str()) {
            return None;
        }
        (dir, stem)
    };
    if !PROBE_DIRS.contains(&dir) {
        return None;
    }
    CANDIDATE_PATHS.iter().position(|candidate| {
        let (candidate_dir, candidate_name) = split_path(candidate);
        candidate_dir == dir && file_stem(candidate_name) == stem
    })
}

/// `dir/name`, or `name` at the root.
fn join_path(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{dir}/{name}")
    }
}

/// `repo/path` of every found file whose text is not in `states`.
fn missing_text(states: &[SourceState]) -> Vec<String> {
    states
        .iter()
        .flat_map(|state| state.rows.iter().flatten())
        .filter_map(|row| row.file.as_ref())
        .filter(|file| file.text.is_none())
        .map(|file| format!("{}/{}", file.repo, file.path))
        .collect()
}

/// How one source repository was checked.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicySource {
    pub repo: RepoName,
    /// False when the repository does not exist or is empty.
    pub exists: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// `cache` (no request), `network`, or `not_modified`.
    pub origin: Origin,
    pub checked_at: DateTime<Utc>,
}

/// A candidate path that does not exist in a source (cached as absent).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AbsentPath {
    pub repo: RepoName,
    pub path: String,
    /// The commit-pinned location that was checked, when the source exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// Full result of a fetch: files plus how each source was checked.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyFetch {
    pub repo: RepoName,
    pub sources: Vec<PolicySource>,
    /// Found files: org files first, then the repository's, in
    /// [`CANDIDATE_PATHS`] order.
    pub files: Vec<PolicyFile>,
    pub absent: Vec<AbsentPath>,
    /// Stale-cache, missing-text and symlink notes for `meta.warnings`
    /// (sets `meta.partial`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

pub struct GitHubPolicyFiles {
    pub gateway: Arc<Gateway>,
    pub store: SharedStore,
    pub config: Config,
}

#[derive(Clone, Copy)]
enum Read {
    Cached(Freshness),
    Live(LiveRead),
}

/// Per-source working state.
struct SourceState {
    repo: RepoName,
    status: u16,
    commit: Option<String>,
    etag: Option<String>,
    origin: Origin,
    checked_at: DateTime<Utc>,
    /// Rows cached for this commit (marker excluded), when still valid.
    rows: Option<Vec<CachedPolicyFile>>,
    /// Rows from an older snapshot, kept for stale fallback.
    previous: Vec<CachedPolicyFile>,
    /// Marker or rows must be written back.
    dirty: bool,
}

impl SourceState {
    fn exists(&self) -> bool {
        self.status == 200 && self.commit.is_some()
    }
}

fn lock(store: &SharedStore) -> MutexGuard<'_, Store> {
    store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Found blob in a listing.
struct Match {
    /// Git mode 120000: the blob's text is the link target.
    symlink: bool,
    path: String,
    oid: String,
    size: u64,
}

impl GitHubPolicyFiles {
    pub fn new(gateway: Arc<Gateway>, store: SharedStore, config: Config) -> Self {
        Self {
            gateway,
            store,
            config,
        }
    }

    /// [`PolicyFiles::fetch`] with source details, absent paths and warnings.
    pub fn fetch_report(&self, repo: &RepoName, mode: Freshness) -> Result<PolicyFetch> {
        let read = match mode.live() {
            Some(live) => Read::Live(live),
            None => Read::Cached(mode),
        };
        self.run(repo, read, true)
    }

    fn run(&self, repo: &RepoName, read: Read, want_text: bool) -> Result<PolicyFetch> {
        let mut warnings = Vec::new();
        let mut states = Vec::new();
        for source in policy_sources(repo) {
            states.push(self.probe(&source, read, &mut warnings)?);
        }

        // Sources whose HEAD moved or that were never listed.
        let to_list: Vec<usize> = (0..states.len())
            .filter(|&i| states[i].exists() && states[i].rows.is_none())
            .collect();
        if !to_list.is_empty() {
            match self.list(&mut states, &to_list, want_text) {
                Ok(()) => {}
                Err(error) if matches!(read, Read::Cached(_)) => {
                    // Serve the previous snapshot rather than nothing.
                    let mut served = false;
                    let mut unserved = Vec::new();
                    for &i in &to_list {
                        // Either way nothing is cached for the new commit.
                        states[i].dirty = false;
                        if states[i].previous.is_empty() {
                            unserved.push(states[i].repo.to_string());
                        } else {
                            states[i].rows = Some(std::mem::take(&mut states[i].previous));
                            served = true;
                        }
                    }
                    if !served {
                        return Err(error);
                    }
                    warnings.push(format!(
                        "listing policy files failed ({error:#}); served the previously cached files"
                    ));
                    if !unserved.is_empty() {
                        warnings.push(format!(
                            "{} had no previously cached policy files; none are listed for it",
                            unserved.join(", ")
                        ));
                    }
                }
                Err(error) => return Err(error),
            }
        }

        if want_text {
            match read {
                // Cache-only: report the missing text instead of fetching it.
                Read::Cached(Freshness::Offline) => {
                    let missing = missing_text(&states);
                    if !missing.is_empty() {
                        warnings.push(format!(
                            "policy file text is not cached for {}; run without --offline to fetch it",
                            missing.join(", ")
                        ));
                    }
                }
                Read::Cached(_) => {
                    if let Err(error) = self.fill_missing_text(&mut states) {
                        warnings.push(format!(
                            "fetching policy file text failed ({error:#}); served {} without text",
                            missing_text(&states).join(", ")
                        ));
                    }
                }
                Read::Live(_) => self.fill_missing_text(&mut states)?,
            }
        }

        {
            let mut store = lock(&self.store);
            for state in states.iter().filter(|s| s.dirty) {
                let mut rows = vec![CachedPolicyFile {
                    repo: state.repo.clone(),
                    path: POLICY_SNAPSHOT_PATH.into(),
                    status: state.status,
                    file: None,
                    commit: state.commit.clone(),
                    url: Some(format!("https://github.com/{}", state.repo)),
                    etag: state.etag.clone(),
                    fetched_at: state.checked_at,
                }];
                // Restamp every row with the check time: the snapshot is
                // all-or-nothing, so `cache prune` (by `fetched_at`) must never
                // keep the marker while dropping its file / absent rows.
                rows.extend(state.rows.iter().flatten().cloned().map(|mut row| {
                    row.fetched_at = state.checked_at;
                    if let Some(file) = row.file.as_mut() {
                        file.fetched_at = state.checked_at;
                    }
                    row
                }));
                store.replace_policy_files(&state.repo, &rows)?;
            }
        }

        let mut files = Vec::new();
        let mut absent = Vec::new();
        let mut sources = Vec::new();
        for state in &states {
            sources.push(PolicySource {
                repo: state.repo.clone(),
                exists: state.exists(),
                commit: state.commit.clone(),
                origin: state.origin,
                checked_at: state.checked_at,
            });
            if !state.exists() {
                absent.extend(unique_candidates().map(|path| AbsentPath {
                    repo: state.repo.clone(),
                    path: path.into(),
                    url: None,
                }));
                continue;
            }
            let mut found: Vec<PolicyFile> = Vec::new();
            for row in state.rows.iter().flatten() {
                match &row.file {
                    Some(file) => {
                        let mut file = file.clone();
                        file.fetched_at = state.checked_at;
                        if row.etag.as_deref() == Some(SYMLINK_TAG) {
                            let target = file
                                .text
                                .as_deref()
                                .map(|t| format!(" to `{}`", t.trim()))
                                .unwrap_or_default();
                            warnings.push(format!(
                                "{}/{} is a symbolic link{target}; its text is the link target, not a policy: read the target file",
                                file.repo, file.path
                            ));
                        }
                        if !want_text {
                            file.text = None;
                        }
                        found.push(file);
                    }
                    None => absent.push(AbsentPath {
                        repo: state.repo.clone(),
                        path: row.path.clone(),
                        url: row.url.clone(),
                    }),
                }
            }
            found.sort_by(|a, b| {
                (candidate_rank(&a.path), &a.path).cmp(&(candidate_rank(&b.path), &b.path))
            });
            files.extend(found);
        }
        Ok(PolicyFetch {
            repo: repo.clone(),
            sources,
            files,
            absent,
            warnings,
        })
    }

    /// Step 1: decide the source's HEAD commit, from the marker or a request.
    fn probe(
        &self,
        source: &RepoName,
        read: Read,
        warnings: &mut Vec<String>,
    ) -> Result<SourceState> {
        let (marker, cached_rows) = {
            let store = lock(&self.store);
            let rows = store.cached_policy_files(source)?;
            split_marker(rows)
        };
        let ttl = self.config.ttls().policy_files;
        let now = Utc::now();
        // An existing source always caches one row per candidate (found or
        // absent). A marker for an existing source with no such rows is an
        // incomplete snapshot (e.g. pruned by an older version): a miss.
        let marker = marker.filter(|m| m.data.status != 200 || !cached_rows.is_empty());

        if let Read::Cached(mode) = read {
            // A marker dated in the future (clock skew) is stale, not young.
            let young = marker
                .as_ref()
                .is_some_and(|m| (now - m.fetched_at).to_std().is_ok_and(|age| age < ttl));
            if mode == Freshness::Offline || (mode == Freshness::Default && young) {
                let Some(marker) = marker else {
                    return Err(ScoutError::NotFound {
                        what: format!(
                            "cached policy files for {source} (offline); run without --offline first"
                        ),
                    }
                    .into());
                };
                return Ok(SourceState {
                    repo: source.clone(),
                    status: marker.data.status,
                    commit: marker.data.commit.clone(),
                    etag: marker.data.etag.clone(),
                    origin: Origin::Cache,
                    checked_at: marker.fetched_at,
                    rows: Some(cached_rows),
                    previous: Vec::new(),
                    dirty: false,
                });
            }
        }

        let request =
            ApiRequest::get(format!("/repos/{source}/commits/HEAD")).accept(SHA_MEDIA_TYPE);
        let answer = match read {
            Read::Live(live) => self.gateway.get_live(&request, live).map(|live| {
                let origin = if live.not_modified() {
                    Origin::NotModified
                } else {
                    Origin::Network
                };
                let at = live.as_of();
                (live.into_inner(), origin, at)
            }),
            Read::Cached(mode) => self.gateway.get(&request, mode, ttl).map(|fetched| {
                if let Some(note) = fetched.stale {
                    warnings.push(note);
                }
                (fetched.response, fetched.origin, fetched.fetched_at)
            }),
        };
        let (response, origin, checked_at) = match answer {
            Ok(answer) => answer,
            // An empty repository answers 409: nothing to read.
            Err(error)
                if matches!(
                    ScoutError::find(&error),
                    Some(ScoutError::HttpStatus { status: 409, .. })
                ) =>
            {
                (ApiResponse::new(409, Vec::new()), Origin::Network, now)
            }
            Err(error) => {
                return Err(error.context(format!("check {source} HEAD for policy files")))
            }
        };

        let mut state = SourceState {
            repo: source.clone(),
            status: response.status,
            commit: None,
            etag: response.etag().map(str::to_owned),
            origin,
            checked_at,
            rows: None,
            previous: Vec::new(),
            dirty: true,
        };
        if !response.is_success() {
            // 404 / 409: the source has no files.
            state.status = response.status;
            state.rows = Some(Vec::new());
            return Ok(state);
        }
        state.status = 200;
        let commit =
            parse_commit(&response).with_context(|| format!("read {source} HEAD commit"))?;
        let same = marker
            .as_ref()
            .is_some_and(|m| m.data.status == 200 && m.data.commit.as_deref() == Some(&commit));
        state.commit = Some(commit);
        if same {
            state.rows = Some(cached_rows);
        } else {
            state.previous = cached_rows;
        }
        Ok(state)
    }

    /// Step 2: one GraphQL query listing every source in `indices`.
    fn list(&self, states: &mut [SourceState], indices: &[usize], want_text: bool) -> Result<()> {
        let mut decls = Vec::new();
        let mut body = String::new();
        let mut vars = Map::new();
        for (slot, &i) in indices.iter().enumerate() {
            let state = &states[i];
            let commit = state.commit.as_deref().unwrap_or("HEAD");
            decls.push(format!("$o{slot}: String!, $n{slot}: String!"));
            vars.insert(format!("o{slot}"), json!(state.repo.owner()));
            vars.insert(format!("n{slot}"), json!(state.repo.name()));
            body.push_str(&format!(
                "  s{slot}: repository(owner: $o{slot}, name: $n{slot}) {{\n"
            ));
            for (k, dir) in PROBE_DIRS.iter().enumerate() {
                decls.push(format!("$d{slot}_{k}: String!"));
                vars.insert(format!("d{slot}_{k}"), json!(format!("{commit}:{dir}")));
                body.push_str(&format!(
                    "    d{k}: object(expression: $d{slot}_{k}) {{ ...tree }}\n"
                ));
                for (c, name) in TEMPLATE_DIRS.iter().enumerate() {
                    decls.push(format!("$t{slot}_{k}_{c}: String!"));
                    vars.insert(
                        format!("t{slot}_{k}_{c}"),
                        json!(format!("{commit}:{}", join_path(dir, name))),
                    );
                    body.push_str(&format!(
                        "    t{k}_{c}: object(expression: $t{slot}_{k}_{c}) {{ ...tree }}\n"
                    ));
                }
            }
            if want_text {
                for (j, path) in CANDIDATE_PATHS.iter().enumerate() {
                    decls.push(format!("$f{slot}_{j}: String!"));
                    vars.insert(format!("f{slot}_{j}"), json!(format!("{commit}:{path}")));
                    body.push_str(&format!(
                        "    f{j}: object(expression: $f{slot}_{j}) {{ ...blob }}\n"
                    ));
                }
            }
            body.push_str("  }\n");
        }
        let mut query = format!("query({}) {{\n{body}}}\n", decls.join(", "));
        query.push_str(TREE_FRAGMENT);
        if want_text {
            query.push_str(BLOB_FRAGMENT);
        }
        let data = graphql_data(&self.gateway.graphql(&query, &Value::Object(vars))?)?;
        let max = self.config.policy_text_max_bytes as usize;

        for (slot, &i) in indices.iter().enumerate() {
            let state = &mut states[i];
            let commit = state.commit.clone().unwrap_or_default();
            let repo_data = data
                .get(format!("s{slot}"))
                .filter(|v| !v.is_null())
                .ok_or_else(|| anyhow!("GraphQL returned no repository {}", state.repo))?;
            // Every listed tree with its path: each probed directory, then
            // its template directories.
            let entries_of = |alias: String| {
                repo_data
                    .get(alias)
                    .and_then(|tree| tree.get("entries"))
                    .and_then(Value::as_array)
            };
            let mut listed = Vec::new();
            for (k, dir) in PROBE_DIRS.iter().enumerate() {
                listed.push((dir.to_string(), entries_of(format!("d{k}"))));
                for (c, name) in TEMPLATE_DIRS.iter().enumerate() {
                    listed.push((join_path(dir, name), entries_of(format!("t{k}_{c}"))));
                }
            }
            let mut matches = Vec::new();
            for (dir, entries) in listed {
                for entry in entries.into_iter().flatten() {
                    if entry.get("type").and_then(Value::as_str) != Some("blob") {
                        continue;
                    }
                    let Some(name) = entry.get("name").and_then(Value::as_str) else {
                        continue;
                    };
                    let path = join_path(&dir, name);
                    if candidate_rank(&path).is_none() {
                        continue;
                    }
                    matches.push(Match {
                        symlink: entry.get("mode").and_then(Value::as_u64) == Some(SYMLINK_MODE),
                        path,
                        oid: entry
                            .get("oid")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        size: entry
                            .pointer("/object/byteSize")
                            .and_then(Value::as_u64)
                            .unwrap_or_default(),
                    });
                }
            }
            let exact_text = |path: &str, oid: &str| -> Option<(String, bool, u64)> {
                let j = CANDIDATE_PATHS.iter().position(|c| *c == path)?;
                let blob = repo_data.get(format!("f{j}")).filter(|v| !v.is_null())?;
                if blob.get("oid").and_then(Value::as_str) != Some(oid) {
                    return None;
                }
                Some(blob_text(blob, max))
            };

            let mut rows = Vec::new();
            for found in &matches {
                let (text, truncated, size) = match want_text
                    .then(|| exact_text(&found.path, &found.oid))
                    .flatten()
                {
                    Some((text, truncated, size)) => (Some(text), truncated, size),
                    None => (None, false, found.size),
                };
                let url = pinned_url(&state.repo, &commit, &found.path);
                rows.push(CachedPolicyFile {
                    repo: state.repo.clone(),
                    path: found.path.clone(),
                    status: 200,
                    file: Some(PolicyFile {
                        repo: state.repo.clone(),
                        path: found.path.clone(),
                        url: url.clone(),
                        text,
                        truncated,
                        size,
                        sha: Some(found.oid.clone()),
                        fetched_at: state.checked_at,
                    }),
                    commit: None,
                    url: Some(url),
                    etag: found.symlink.then(|| SYMLINK_TAG.to_owned()),
                    fetched_at: state.checked_at,
                });
            }
            // Absent candidates, one row per case-insensitive path, unless a
            // found file stands for it (another extension, a template directory).
            for path in unique_candidates() {
                let covered = matches.iter().any(|m| {
                    candidate_rank(&m.path)
                        .is_some_and(|rank| CANDIDATE_PATHS[rank].eq_ignore_ascii_case(path))
                });
                if covered {
                    continue;
                }
                rows.push(CachedPolicyFile {
                    repo: state.repo.clone(),
                    path: (*path).into(),
                    status: 404,
                    file: None,
                    commit: Some(commit.clone()),
                    url: Some(pinned_url(&state.repo, &commit, path)),
                    etag: None,
                    fetched_at: state.checked_at,
                });
            }
            state.rows = Some(rows);
            state.dirty = true;
        }
        Ok(())
    }

    /// Step 3: one GraphQL query for found files whose text is not cached.
    fn fill_missing_text(&self, states: &mut [SourceState]) -> Result<()> {
        // (state index, row index, oid)
        let mut wanted: Vec<(usize, usize, String)> = Vec::new();
        for (i, state) in states.iter().enumerate() {
            for (r, row) in state.rows.iter().flatten().enumerate() {
                if let Some(file) = &row.file {
                    if file.text.is_none() {
                        if let Some(oid) = &file.sha {
                            wanted.push((i, r, oid.clone()));
                        }
                    }
                }
            }
        }
        if wanted.is_empty() {
            return Ok(());
        }
        let mut by_source: BTreeMap<usize, Vec<(usize, String)>> = BTreeMap::new();
        for (i, r, oid) in wanted {
            by_source.entry(i).or_default().push((r, oid));
        }
        let mut decls = Vec::new();
        let mut body = String::new();
        let mut vars = Map::new();
        for (slot, (i, blobs)) in by_source.iter().enumerate() {
            let repo = &states[*i].repo;
            decls.push(format!("$o{slot}: String!, $n{slot}: String!"));
            vars.insert(format!("o{slot}"), json!(repo.owner()));
            vars.insert(format!("n{slot}"), json!(repo.name()));
            body.push_str(&format!(
                "  s{slot}: repository(owner: $o{slot}, name: $n{slot}) {{\n"
            ));
            for (b, (_, oid)) in blobs.iter().enumerate() {
                decls.push(format!("$b{slot}_{b}: GitObjectID!"));
                vars.insert(format!("b{slot}_{b}"), json!(oid));
                body.push_str(&format!(
                    "    b{b}: object(oid: $b{slot}_{b}) {{ ...blob }}\n"
                ));
            }
            body.push_str("  }\n");
        }
        let query = format!("query({}) {{\n{body}}}\n{BLOB_FRAGMENT}", decls.join(", "));
        let data = graphql_data(&self.gateway.graphql(&query, &Value::Object(vars))?)?;
        let max = self.config.policy_text_max_bytes as usize;
        for (slot, (i, blobs)) in by_source.iter().enumerate() {
            let state = &mut states[*i];
            for (b, (r, _)) in blobs.iter().enumerate() {
                let blob = data
                    .pointer(&format!("/s{slot}/b{b}"))
                    .filter(|v| !v.is_null())
                    .ok_or_else(|| {
                        anyhow!(
                            "GraphQL returned no blob for a policy file in {}",
                            state.repo
                        )
                    })?;
                let (text, truncated, size) = blob_text(blob, max);
                if let Some(file) = state
                    .rows
                    .as_mut()
                    .and_then(|rows| rows.get_mut(*r))
                    .and_then(|row| row.file.as_mut())
                {
                    file.text = Some(text);
                    file.truncated = truncated;
                    file.size = size;
                }
            }
            state.dirty = true;
        }
        Ok(())
    }
}

const TREE_FRAGMENT: &str =
    "fragment tree on Tree { entries { name type mode oid object { ... on Blob { byteSize } } } }\n";
const BLOB_FRAGMENT: &str = "fragment blob on Blob { oid byteSize isTruncated text }\n";

/// `(text, truncated, size)` of a GraphQL `Blob`. Binary blobs have no text.
fn blob_text(blob: &Value, max: usize) -> (String, bool, u64) {
    let size = blob
        .get("byteSize")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let server_truncated = blob
        .get("isTruncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let raw = blob.get("text").and_then(Value::as_str);
    let (text, truncated) = truncate_text(raw.unwrap_or_default(), max);
    let truncated = truncated || server_truncated || (raw.is_none() && size > 0);
    (text, truncated, size)
}

/// `data` of a GraphQL response; any `errors` fail the call.
pub(crate) fn graphql_data(response: &ApiResponse) -> Result<Value> {
    let body: Value = response.json_body().context("parse GraphQL response")?;
    if let Some(errors) = body
        .get("errors")
        .and_then(Value::as_array)
        .filter(|errors| !errors.is_empty())
    {
        let messages: Vec<&str> = errors
            .iter()
            .filter_map(|e| e.get("message").and_then(Value::as_str))
            .collect();
        bail!("GitHub GraphQL errors: {}", messages.join("; "));
    }
    body.get("data")
        .filter(|data| !data.is_null())
        .cloned()
        .ok_or_else(|| anyhow!("GraphQL response has no data"))
}

/// The commit SHA from a `commits/HEAD` answer: the bare SHA (sha media
/// type), or a JSON commit object if the media type was ignored.
fn parse_commit(response: &ApiResponse) -> Result<String> {
    let text = response.text()?.trim();
    let is_sha = |s: &str| (40..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_hexdigit());
    if is_sha(text) {
        return Ok(text.to_ascii_lowercase());
    }
    let value: Value = serde_json::from_str(text).context("expected a commit SHA")?;
    match value.get("sha").and_then(Value::as_str) {
        Some(sha) if is_sha(sha) => Ok(sha.to_ascii_lowercase()),
        _ => bail!("expected a commit SHA"),
    }
}

/// Splits a source's cached rows into the marker and the rest.
fn split_marker(
    rows: Vec<Cached<CachedPolicyFile>>,
) -> (Option<Cached<CachedPolicyFile>>, Vec<CachedPolicyFile>) {
    let mut marker = None;
    let mut rest = Vec::new();
    for row in rows {
        if row.data.path == POLICY_SNAPSHOT_PATH {
            marker = Some(row);
        } else {
            rest.push(row.data);
        }
    }
    (marker, rest)
}

impl PolicyFiles for GitHubPolicyFiles {
    fn fetch(&self, repo: &RepoName, mode: Freshness) -> Result<Vec<PolicyFile>> {
        Ok(self.fetch_report(repo, mode)?.files)
    }

    fn fetch_live(
        &self,
        repo: &RepoName,
        live: LiveRead,
        with_text: bool,
    ) -> Result<Live<Vec<PolicyFile>>> {
        let report = self.run(repo, Read::Live(live), with_text)?;
        let as_of = report
            .sources
            .iter()
            .map(|s| s.checked_at)
            .min()
            .unwrap_or_else(Utc::now);
        let not_modified = report
            .sources
            .iter()
            .all(|s| s.origin == Origin::NotModified);
        Ok(Live::new(report.files, as_of, not_modified))
    }
}
