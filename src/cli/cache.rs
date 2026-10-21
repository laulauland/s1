//! `s1 util cache clear`: remove every cached response. Through the daemon when
//! it runs (it holds the store open); else the store file is deleted.

use std::process::ExitCode;

use axum::http::{HeaderMap, Method};
use bytes::Bytes;
use clap::Subcommand;
use serde_json::Value;

use super::client::{is_running, request};
use crate::paths::Paths;

#[derive(Debug, Subcommand)]
pub enum CacheCmd {
    /// Remove every cached response.
    Clear,
}

pub fn run(cmd: CacheCmd, paths: &Paths) -> Result<ExitCode, String> {
    match cmd {
        CacheCmd::Clear => clear(paths),
    }
}

fn clear(paths: &Paths) -> Result<ExitCode, String> {
    if is_running(&paths.socket) {
        let reply = super::runtime()?.block_on(request(
            &paths.socket,
            Method::POST,
            "/cache/clear",
            HeaderMap::new(),
            Bytes::new(),
        ))?;
        if !reply.status.is_success() {
            return Err(format!(
                "daemon returned {}: {}",
                reply.status,
                String::from_utf8_lossy(&reply.body)
            ));
        }
        let v: Value =
            serde_json::from_slice(&reply.body).map_err(|e| format!("bad daemon reply: {e}"))?;
        println!(
            "cleared {} cached responses",
            v["cleared"].as_u64().unwrap_or(0)
        );
        return Ok(ExitCode::SUCCESS);
    }
    let file = paths.cache_file();
    match std::fs::remove_file(&file) {
        Ok(()) => println!("removed {}", file.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => println!("cache is empty"),
        Err(e) => return Err(format!("cannot remove {}: {e}", file.display())),
    }
    Ok(ExitCode::SUCCESS)
}
