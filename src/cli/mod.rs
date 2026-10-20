//! The `s1` command line. One file per command group; new verbs (decide,
//! extract, forecast) add a variant to [`Cmd`] and a module.

use std::process::ExitCode;

use clap::{Parser, Subcommand};

use crate::paths::Paths;

mod auth;
mod cache;
pub mod client;
mod daemon;
pub mod decide;
mod models;
mod raw;
mod status;
pub mod stream;

#[derive(Debug, Parser)]
#[command(name = "s1", version, about = "Local gateway for System One models")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Start, stop, or inspect the daemon.
    Daemon {
        #[command(subcommand)]
        cmd: daemon::DaemonCmd,
    },
    /// Send stdin to a provider unchanged and print its response unchanged.
    Raw(raw::RawArgs),
    /// List models, aliases, and their state.
    Models(models::ModelsArgs),
    /// Ask Decide questions: one text, or one JSON line in, one line out.
    Decide(decide::DecideArgs),
    /// Queues, limits, and breakers per backend; cache size.
    Status(status::StatusArgs),
    /// Manage the response cache.
    Cache {
        #[command(subcommand)]
        cmd: cache::CacheCmd,
    },
    /// Manage provider keys in ~/.s1/auth.json.
    Auth {
        #[command(subcommand)]
        cmd: auth::AuthCmd,
    },
}

/// Entry point of the binary.
pub fn main() -> ExitCode {
    let cli = Cli::parse();
    let paths = Paths::from_env();
    let result = match cli.cmd {
        Cmd::Daemon { cmd } => daemon::run(cmd, &paths),
        Cmd::Raw(args) => raw::run(args, &paths),
        Cmd::Models(args) => models::run(args, &paths),
        Cmd::Auth { cmd } => auth::run(cmd, &paths),
        Cmd::Decide(args) => decide::run(args, &paths),
        Cmd::Status(args) => status::run(args, &paths),
        Cmd::Cache { cmd } => cache::run(cmd, &paths),
    };
    match result {
        Ok(code) => code,
        Err(msg) => {
            eprintln!("s1: {msg}");
            ExitCode::FAILURE
        }
    }
}

/// A small runtime for one CLI command.
pub(crate) fn runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("cannot start runtime: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_is_well_formed() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_all_subcommands() {
        for args in [
            &["s1", "daemon", "start"][..],
            &["s1", "daemon", "stop"],
            &["s1", "daemon", "logs", "-n", "5"],
            &["s1", "raw", "systemone"],
            &[
                "s1",
                "raw",
                "systemone",
                "--provider",
                "typesafe",
                "-H",
                "A: b",
            ],
            &["s1", "models", "--json"],
            &["s1", "auth", "set", "typesafe"],
            &["s1", "auth", "set", "workers-ai", "--account-id", "x"],
            &["s1", "auth", "list"],
            &["s1", "auth", "token"],
            &["s1", "status"],
            &["s1", "status", "--json"],
            &["s1", "cache", "clear"],
            &["s1", "decide", "-t", "t.json", "--no-cache", "text"],
            &["s1", "decide", "-t", "t.json", "text"],
            &["s1", "decide", "--openai", "-t", "t.json", "text"],
            &[
                "s1",
                "decide",
                "-t",
                "t.json",
                "--jsonl",
                "--flat",
                "--id-field",
                "key",
                "--state-field",
                "body",
                "--image-field",
                "photo",
                "--keep",
                "*",
                "--unordered",
                "-j",
                "4",
                "-m",
                "decide",
            ],
        ] {
            Cli::try_parse_from(args).unwrap_or_else(|e| panic!("{args:?}: {e}"));
        }
    }
}
