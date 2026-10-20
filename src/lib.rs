//! s1: a local gateway daemon and CLI for System One models.
//!
//! Module map (one module per concern, so later stages add files, not edits):
//! - [`paths`] - file locations (`~/.s1`, XDG dirs).
//! - [`config`] - `~/.config/s1/config.toml`.
//! - [`catalog`] - compiled-in catalog plus `models.toml` overlay.
//! - [`secrets`] - `~/.s1/auth.json` and env var keys.
//! - [`protocol`] - one module per wire format ([`protocol::Protocol`]).
//! - [`backend`] - one module per provider ([`backend::Backend`]).
//! - [`translate`] - format translation inside one family ([`translate::Translate`]).
//! - [`router`] - picks a backend by the `model` field (ids and aliases).
//! - [`limits`] - token buckets, AIMD cap, rate-limit headers ([`limits::Gate`]).
//! - [`breaker`] - circuit breaker per backend and key.
//! - [`retry`] - retry classification and backoff.
//! - [`queue`] - bounded queue with lanes and the async [`queue::Scheduler`].
//! - [`protect`] - one scheduler per backend and key, and the guarded call.
//! - [`cache`] - response cache (blake3 key, redb store, model versions).
//! - [`flatten`] - `--flat` column names and template name checks.
//! - [`images`] - image file paths in systemone `assets` (Unix socket only).
//! - [`mcp`] - stateless MCP endpoint (`POST /mcp`) and its tools.
//! - [`daemon`] - the axum router, its handlers, and the listeners.
//! - [`cli`] - the `s1` command line.

pub mod backend;
pub mod breaker;
pub mod cache;
pub mod catalog;
pub mod cli;
pub mod config;
pub mod daemon;
pub mod error;
pub mod flatten;
pub mod images;
pub mod limits;
pub mod logging;
pub mod mcp;
pub mod paths;
pub mod protect;
pub mod protocol;
pub mod queue;
pub mod retry;
pub mod router;
pub mod secrets;
pub mod translate;
