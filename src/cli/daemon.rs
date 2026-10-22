//! `s1 util daemon start|stop|logs` (and the hidden `run`, the daemon itself).

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
            // SAFETY: no other thread exists yet; the runtime starts below.
            let inherited = unsafe { crate::daemon::activation::take() }?;
            let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
            rt.block_on(crate::daemon::run(paths.clone(), inherited))
                .map_err(|e| e.to_string())?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// The pid of the daemon that answers on the socket (`GET /health`), and
/// whether systemd passed it its listeners. Only the socket owner can
/// connect, so this names the process to signal; the pid in `daemon.json`
/// may be stale and belong to an unrelated process.
fn daemon_pid(paths: &Paths) -> Result<(i32, bool), String> {
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
    let pid = body["pid"]
        .as_u64()
        .and_then(|p| i32::try_from(p).ok())
        .filter(|p| *p > 0)
        .ok_or_else(|| "daemon /health answer has no pid".to_string())?;
    Ok((pid, body["socket_activated"] == true))
}

fn stop(paths: &Paths) -> Result<ExitCode, String> {
    if !is_running(&paths.socket) {
        let _ = std::fs::remove_file(paths.daemon_file());
        println!("s1 daemon is not running");
        return Ok(ExitCode::SUCCESS);
    }
    let (pid, socket_activated) = daemon_pid(paths)?;
    // systemd keeps the socket and starts a new daemon for the next
    // connection, so a SIGTERM alone does not stop it.
    if socket_activated {
        return Err(format!(
            "the daemon (pid {pid}) is managed by systemd; stop it with \
             `systemctl --user stop s1.socket s1.service`"
        ));
    }
    // SAFETY: plain kill(2) with the pid the daemon reported over its socket.
    if unsafe { libc::kill(pid, libc::SIGTERM) } != 0 {
        return Err(format!(
            "cannot signal pid {pid}: {}",
            std::io::Error::last_os_error()
        ));
    }
    // Wait for the process to exit, not only for the socket to close: open
    // requests drain first (up to 120 s, as `TimeoutStopSec` in s1.service),
    // and the daemon holds the cache file until it exits.
    let gone = || {
        // SAFETY: kill(2) with signal 0 only checks that the pid exists.
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        if !alive {
            return std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
        }
        // An exited process its parent has not reaped yet still has a pid.
        zombie(pid)
    };
    let deadline = Instant::now() + Duration::from_secs(130);
    while Instant::now() < deadline {
        if gone() {
            println!("s1 daemon stopped");
            return Ok(ExitCode::SUCCESS);
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    Err(format!("daemon (pid {pid}) did not exit within 130 s"))
}

/// True if `pid` has exited and waits to be reaped (Linux `/proc`; false
/// where that is not available).
fn zombie(pid: i32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    // `pid (comm) state ...`; comm can hold spaces and parentheses.
    stat.rsplit_once(')')
        .and_then(|(_, rest)| rest.split_whitespace().next())
        == Some("Z")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn an_exited_unreaped_child_counts_as_a_zombie() {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id() as i32;
        let deadline = Instant::now() + Duration::from_secs(5);
        while !zombie(pid) {
            assert!(Instant::now() < deadline, "child never became a zombie");
            std::thread::sleep(Duration::from_millis(10));
        }
        child.wait().unwrap();
        assert!(!zombie(pid));
        assert!(!zombie(std::process::id() as i32));
    }
}
