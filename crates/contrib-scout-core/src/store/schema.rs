//! SQLite schema (§6). Migrations only move forward; never edit a shipped one.
//!
//! Conventions: timestamps are INTEGER Unix seconds (UTC); repository names are
//! `owner/name` compared with `COLLATE NOCASE`; issue refs are `owner/name#N`;
//! structured fields are JSON text columns ending in `_json`.

/// Schema version 1: every table in the design.
pub const V1: &str = "
CREATE TABLE meta (
    key TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL
);

CREATE TABLE repos (
    repo TEXT PRIMARY KEY NOT NULL COLLATE NOCASE,
    owner TEXT NOT NULL COLLATE NOCASE,
    archived INTEGER NOT NULL DEFAULT 0,
    fork INTEGER NOT NULL DEFAULT 0,
    license TEXT,
    default_branch TEXT,
    language TEXT COLLATE NOCASE,
    description TEXT,
    pushed_at INTEGER,
    stars INTEGER NOT NULL DEFAULT 0,
    topics_json TEXT NOT NULL DEFAULT '[]',
    raw_json TEXT,
    etag TEXT,
    fetched_at INTEGER NOT NULL
);
CREATE INDEX repos_owner ON repos(owner);
CREATE INDEX repos_language ON repos(language);
CREATE INDEX repos_fetched ON repos(fetched_at);

CREATE TABLE policy_marks (
    target TEXT PRIMARY KEY NOT NULL COLLATE NOCASE,
    target_kind TEXT NOT NULL CHECK (target_kind IN ('repo', 'org')),
    stance TEXT NOT NULL,
    scope_json TEXT NOT NULL DEFAULT '{}',
    requirements_json TEXT NOT NULL DEFAULT '[]',
    evidence_json TEXT NOT NULL,
    note TEXT NOT NULL,
    marked_by TEXT NOT NULL,
    marked_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL
);
CREATE INDEX policy_marks_stance ON policy_marks(stance);
CREATE INDEX policy_marks_expires ON policy_marks(expires_at);

CREATE TABLE policy_mark_history (
    id INTEGER PRIMARY KEY,
    target TEXT NOT NULL COLLATE NOCASE,
    target_kind TEXT NOT NULL CHECK (target_kind IN ('repo', 'org')),
    action TEXT NOT NULL,
    mark_json TEXT,
    agent_id TEXT NOT NULL,
    at INTEGER NOT NULL
);
CREATE INDEX policy_mark_history_target ON policy_mark_history(target, id);

CREATE TABLE policy_files (
    repo TEXT NOT NULL COLLATE NOCASE,
    path TEXT NOT NULL,
    status INTEGER NOT NULL,
    url TEXT,
    sha TEXT,
    text TEXT,
    size INTEGER NOT NULL DEFAULT 0,
    truncated INTEGER NOT NULL DEFAULT 0,
    etag TEXT,
    fetched_at INTEGER NOT NULL,
    PRIMARY KEY (repo, path)
);
CREATE INDEX policy_files_fetched ON policy_files(fetched_at);

CREATE TABLE repo_activity (
    repo TEXT PRIMARY KEY NOT NULL COLLATE NOCASE,
    external_pr_merge_rate REAL,
    median_first_response_secs INTEGER,
    maintainer_last_active INTEGER,
    sample_size INTEGER NOT NULL DEFAULT 0,
    raw_json TEXT,
    computed_at INTEGER NOT NULL
);
CREATE INDEX repo_activity_computed ON repo_activity(computed_at);

CREATE TABLE issues (
    repo TEXT NOT NULL COLLATE NOCASE,
    number INTEGER NOT NULL,
    title TEXT NOT NULL DEFAULT '',
    state TEXT NOT NULL,
    labels_json TEXT NOT NULL DEFAULT '[]',
    assignees_json TEXT NOT NULL DEFAULT '[]',
    comments INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER,
    updated_at INTEGER NOT NULL,
    raw_json TEXT,
    fetched_at INTEGER NOT NULL,
    PRIMARY KEY (repo, number)
);
CREATE INDEX issues_state_updated ON issues(state, updated_at);
CREATE INDEX issues_repo_updated ON issues(repo, updated_at);
CREATE INDEX issues_fetched ON issues(fetched_at);

CREATE TABLE sync_state (
    repo TEXT PRIMARY KEY NOT NULL COLLATE NOCASE,
    since INTEGER,
    last_full_at INTEGER,
    last_sync_at INTEGER,
    state_json TEXT
);

CREATE TABLE http_cache (
    key TEXT PRIMARY KEY NOT NULL,
    etag TEXT,
    last_modified TEXT,
    status INTEGER NOT NULL,
    headers_json TEXT NOT NULL DEFAULT '[]',
    body BLOB,
    fetched_at INTEGER NOT NULL
);
CREATE INDEX http_cache_fetched ON http_cache(fetched_at);

CREATE TABLE quota (
    resource TEXT PRIMARY KEY NOT NULL,
    limit_total INTEGER NOT NULL,
    remaining INTEGER NOT NULL,
    reset_at INTEGER NOT NULL,
    observed_at INTEGER NOT NULL
);

CREATE TABLE request_log (
    id INTEGER PRIMARY KEY,
    at INTEGER NOT NULL,
    resource TEXT NOT NULL,
    agent_id TEXT,
    status INTEGER
);
CREATE INDEX request_log_window ON request_log(resource, at);
CREATE INDEX request_log_at ON request_log(at);

CREATE TABLE claims (
    id INTEGER PRIMARY KEY,
    issue_ref TEXT NOT NULL COLLATE NOCASE,
    agent_id TEXT NOT NULL,
    claimed_at INTEGER NOT NULL,
    lease_until INTEGER NOT NULL,
    status TEXT NOT NULL,
    branch TEXT,
    updated_at INTEGER NOT NULL
);
-- At most one active claim per issue, enforced across processes.
CREATE UNIQUE INDEX claims_one_active ON claims(issue_ref) WHERE status = 'active';
CREATE INDEX claims_agent ON claims(agent_id, status);
CREATE INDEX claims_lease ON claims(status, lease_until);

CREATE TABLE gate_log (
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL,
    issue_ref TEXT NOT NULL COLLATE NOCASE,
    agent_id TEXT NOT NULL,
    verdict TEXT NOT NULL,
    reasons_json TEXT NOT NULL DEFAULT '[]',
    snapshot_json TEXT,
    as_of INTEGER NOT NULL
);
CREATE INDEX gate_log_issue ON gate_log(issue_ref, id);

CREATE TABLE discover_runs (
    query_key TEXT PRIMARY KEY NOT NULL,
    query TEXT NOT NULL,
    params_json TEXT NOT NULL DEFAULT '{}',
    repos_json TEXT NOT NULL DEFAULT '[]',
    fetched_at INTEGER NOT NULL
);
CREATE INDEX discover_runs_fetched ON discover_runs(fetched_at);

CREATE TABLE tracked_repos (
    repo TEXT PRIMARY KEY NOT NULL COLLATE NOCASE,
    added_by TEXT,
    added_at INTEGER NOT NULL
);
";

/// Applied in order; `PRAGMA user_version` and `meta.schema_version` record how many ran.
pub const MIGRATIONS: &[&str] = &[V1];

/// Every table created by [`V1`], for tests and `cache stats`.
pub const TABLES: &[&str] = &[
    "meta",
    "repos",
    "policy_marks",
    "policy_mark_history",
    "policy_files",
    "repo_activity",
    "issues",
    "sync_state",
    "http_cache",
    "quota",
    "request_log",
    "claims",
    "gate_log",
    "discover_runs",
    "tracked_repos",
];
