//! `s1 daemon start|stop|logs` (and the hidden `run`, the daemon itself).

use std::io::Write;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use clap::Subcommand;

use super::client::{ensure_daemon, is_running};
use crate::paths::Paths;

#[derive(Debug, Subcommand)]
pub enum DaemonCmd {
    /// Start the daemon in the background.
    Start,
    /// Stop the running daemon.
    Stop,
    /// Print the end of the newest log file.
    Logs {
        /// Number of lines.
        #[arg(short = 'n', long, default_value_t = 50)]
        lines: usize,
    },
    /// Run the daemon in the foreground.
    #[command(hide = true)]
    Run,
}

pub fn run(cmd: DaemonCmd, paths: &Paths) -> Result<ExitCode, String> {
    match cmd {
        DaemonCmd::Start => {
            if ensure_daemon(paths)? {
                println!("s1 daemon started on {}", paths.socket.display());
            } else {
                println!("s1 daemon is already running on {}", paths.socket.display());
            }
            Ok(ExitCode::SUCCESS)
        }
        DaemonCmd::Stop => stop(paths),
        DaemonCmd::Logs { lines } => logs(paths, lines),
        DaemonCmd::Run => {
            let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
            rt.block_on(crate::daemon::run(paths.clone()))
                .map_err(|e| e.to_string())?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// The pid of the daemon that answers on the socket (`GET /health`). Only the
/// socket owner can connect, so this names the process to signal; the pid in
/// `daemon.json` may be stale and belong to an unrelated process.
fn daemon_pid(paths: &Paths) -> Result<i32, String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let reply = rt.block_on(super::client::request(
        &paths.socket,
        axum::http::Method::GET,
        "/health",
        axum::http::HeaderMap::new(),
        bytes::Bytes::new(),
    ))?;
    let body: serde_json::Value = serde_json::from_slice(&reply.body)
        .map_err(|e| format!("daemon /health answer is not JSON: {e}"))?;
    body["pid"]
        .as_u64()
        .and_then(|p| i32::try_from(p).ok())
        .filter(|p| *p > 0)
        .ok_or_else(|| "daemon /health answer has no pid".to_string())
}

fn stop(paths: &Paths) -> Result<ExitCode, String> {
    if !is_running(&paths.socket) {
        let _ = std::fs::remove_file(paths.daemon_file());
        println!("s1 daemon is not running");
        return Ok(ExitCode::SUCCESS);
    }
    let pid = daemon_pid(paths)?;
    // SAFETY: plain kill(2) with the pid the daemon reported over its socket.
    if unsafe { libc::kill(pid, libc::SIGTERM) } != 0 {
        return Err(format!(
            "cannot signal pid {pid}: {}",
            std::io::Error::last_os_error()
        ));
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if !is_running(&paths.socket) {
            println!("s1 daemon stopped");
            return Ok(ExitCode::SUCCESS);
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    Err(format!("daemon (pid {pid}) did not stop within 10 s"))
}

fn logs(paths: &Paths, lines: usize) -> Result<ExitCode, String> {
    let Some(file) = crate::logging::latest_log(&paths.log_dir()) else {
        return Err(format!("no log files in {}", paths.log_dir().display()));
    };
    let text = std::fs::read_to_string(&file)
        .map_err(|e| format!("cannot read {}: {e}", file.display()))?;
    let all: Vec<&str> = text.lines().collect();
    let start = all.len().saturating_sub(lines);
    let mut out = std::io::stdout().lock();
    for line in &all[start..] {
        let _ = writeln!(out, "{line}");
    }
    Ok(ExitCode::SUCCESS)
}
