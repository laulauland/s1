//! HTTP/1.1 over the daemon's Unix socket, and starting the daemon on first use.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use axum::http::{HeaderMap, Method, Request, StatusCode};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper_util::rt::TokioIo;
use tokio::net::UnixStream;

use crate::paths::Paths;

pub struct Reply {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

/// One request over the Unix socket.
pub async fn request(
    socket: &Path,
    method: Method,
    path: &str,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Reply, String> {
    check_trusted_socket(socket)?;
    let stream = UnixStream::connect(socket)
        .await
        .map_err(|e| format!("cannot connect to {}: {e}", socket.display()))?;
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|e| format!("handshake with daemon failed: {e}"))?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let mut req = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "s1");
    for (k, v) in &headers {
        req = req.header(k, v);
    }
    let req = req
        .body(Full::new(body))
        .map_err(|e| format!("bad request: {e}"))?;
    let resp = sender
        .send_request(req)
        .await
        .map_err(|e| format!("daemon request failed: {e}"))?;
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = resp
        .into_body()
        .collect()
        .await
        .map_err(|e| format!("daemon response failed: {e}"))?
        .to_bytes();
    Ok(Reply {
        status,
        headers,
        body,
    })
}

/// Only a socket of this user in a private dir of this user is trusted with a
/// request: nobody else can have put a listener there.
fn check_trusted_socket(socket: &Path) -> Result<(), String> {
    use crate::secrets::{check_own_file, check_private_dir};
    if let Some(dir) = socket.parent().filter(|d| !d.as_os_str().is_empty()) {
        check_private_dir(dir).map_err(|e| e.to_string())?;
    }
    check_own_file(socket).map_err(|e| e.to_string())?;
    Ok(())
}

/// Make a dir the CLI or daemon writes in (mode 0700, this user's).
fn ensure_private_dir(dir: &Path) -> Result<(), String> {
    crate::secrets::ensure_private_dir(dir).map_err(|e| e.to_string())
}

/// True if a daemon answers on the socket.
pub fn is_running(socket: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(socket).is_ok()
}

/// Start the daemon in the background unless it runs already.
/// Returns `true` if this call started it.
pub fn ensure_daemon(paths: &Paths) -> Result<bool, String> {
    // Secure the dirs before trusting or publishing anything in them (the
    // socket falls back to the state dir when XDG_RUNTIME_DIR is not set).
    ensure_private_dir(&paths.state_dir)?;
    if let Some(dir) = paths.socket.parent().filter(|d| !d.as_os_str().is_empty()) {
        ensure_private_dir(dir)?;
    }
    if is_running(&paths.socket) {
        check_trusted_socket(&paths.socket)?;
        return Ok(false);
    }
    let stderr = std::fs::File::create(paths.daemon_stderr())
        .map_err(|e| format!("cannot create {}: {e}", paths.daemon_stderr().display()))?;
    let exe = std::env::current_exe().map_err(|e| format!("cannot find s1 binary: {e}"))?;
    use std::os::unix::process::CommandExt;
    let mut child = Command::new(exe)
        .args(["util", "daemon", "run"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr)
        .process_group(0)
        .spawn()
        .map_err(|e| format!("cannot start daemon: {e}"))?;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if is_running(&paths.socket) {
            return Ok(true);
        }
        if let Ok(Some(status)) = child.try_wait() {
            let msg = std::fs::read_to_string(paths.daemon_stderr()).unwrap_or_default();
            // Another starter won the lifecycle lock; wait for its socket.
            if msg.contains("already running") {
                return wait_for_socket(&paths.socket, deadline).map(|()| false);
            }
            return Err(format!("daemon exited ({status}): {}", msg.trim()));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    Err("daemon did not start within 10 s".into())
}

fn wait_for_socket(socket: &Path, deadline: Instant) -> Result<(), String> {
    while Instant::now() < deadline {
        if is_running(socket) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    Err("daemon did not start within 10 s".into())
}
