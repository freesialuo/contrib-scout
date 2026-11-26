//! Core library behind the `contrib-scout` CLI (design: `docs/design.md`).
//!
//! Everything except argument parsing and rendering lives here, so a thin MCP
//! layer can be added later (§1). The crate is synchronous: no async runtime.
//!
//! Module map (§3, §4):
//! - [`models`], [`error`], [`freshness`], [`config`], [`envelope`]: shared vocabulary.
//! - [`transport`] → [`gateway`]: the only way out to GitHub.
//! - [`coordinator`]: cross-process single-flight, shared quota, claim leases.
//! - [`store`]: the single shared SQLite database (WAL).
//! - [`policy`], [`profile`], [`discover`], [`index`], [`gate`], [`claims`]: services.

pub mod claims;
pub mod config;
pub mod coordinator;
pub mod discover;
pub mod envelope;
pub mod error;
pub mod freshness;
pub mod gate;
pub mod gateway;
pub mod index;
pub mod models;
pub mod policy;
pub mod profile;
pub mod store;
pub mod transport;

pub use error::{not_implemented, Result, ScoutError};
pub use freshness::{Freshness, Live, LiveRead};
pub use models::{AgentId, IssueRef, MarkTarget, OrgName, RepoName};
