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
//! - [`daemon`] - the axum router, its handlers, and the listeners.
//! - [`cli`] - the `s1` command line.

pub mod backend;
pub mod catalog;
pub mod cli;
pub mod config;
pub mod daemon;
pub mod error;
pub mod logging;
pub mod paths;
pub mod protocol;
pub mod secrets;
pub mod translate;
