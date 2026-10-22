//! systemd socket activation, end to end: the `s1` binary serves listeners
//! it inherits as fds 3 and 4 (both, over HTTP), drains an open request on
//! SIGTERM, answers a connection whose first request comes after the
//! SIGTERM, leaves the socket file alone, and a request sent while no
//! daemon runs is answered by the next daemon on the same listeners.

mod common;

use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::http::{HeaderMap, Method, StatusCode};
use bytes::Bytes;
use common::{Recorder, TEST_KEY, fixture, mock_typesafe_router, overlay_for};
use s1::cli::client::{Reply, request};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Notify;

/// Mock TypeSafe that holds each request: it notifies `entered`, then waits
/// for `release`.
struct Gate {
    addr: std::net::SocketAddr,
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

async fn gated_mock() -> Gate {
    let (entered, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let (e, r) = (entered.clone(), release.clone());
    let app = mock_typesafe_router(Recorder::default()).layer(axum::middleware::from_fn(
        move |req: axum::extract::Request, next: axum::middleware::Next| {
            let (e, r) = (e.clone(), r.clone());
            async move {
                e.notify_one();
                r.notified().await;
                next.run(req).await
            }
        },
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Gate {
        addr,
        entered,
        release,
    }
}

fn s1_env(c: &mut Command, root: &Path) {
    c.env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", root.join("home"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_RUNTIME_DIR", root.join("run"));
}

/// A daemon child in its own process group: the whole group (s1 too, when
/// it runs under a shell) is killed and the child reaped on drop, so a
/// failed assertion does not leave a daemon running.
struct Daemon(Child);

impl Daemon {
    fn spawn(c: &mut Command) -> Self {
        Daemon(c.process_group(0).spawn().unwrap())
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        // SAFETY: kill(2) on the process group of the child we spawned.
        unsafe { libc::kill(-(self.0.id() as i32), libc::SIGKILL) };
        let _ = self.0.wait();
    }
}

/// Start `s1 util daemon run` the way systemd does: the listeners as fds 3
/// and 4, `LISTEN_FDS=2`, and `LISTEN_PID` set to the daemon's own pid (the
/// shell sets it, then `exec`s s1 in the same process).
fn spawn_activated(root: &Path, unix: RawFd, tcp: RawFd) -> Daemon {
    let mut c = Command::new("sh");
    c.args(["-c", "LISTEN_PID=$$ exec \"$0\" util daemon run"])
        .arg(env!("CARGO_BIN_EXE_s1"));
    s1_env(&mut c, root);
    c.env("TYPESAFE_API_KEY", TEST_KEY)
        .env("LISTEN_FDS", "2")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: only async-signal-safe calls (fcntl, dup2) between fork and
    // exec. The fds are first copied above 100 so that moving one to 3 or 4
    // cannot overwrite the other.
    unsafe {
        c.pre_exec(move || {
            let u = libc::fcntl(unix, libc::F_DUPFD, 100);
            let t = libc::fcntl(tcp, libc::F_DUPFD, 100);
            if u < 0 || t < 0 || libc::dup2(u, 3) < 0 || libc::dup2(t, 4) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Daemon::spawn(&mut c)
}

async fn get(socket: &Path, path: &str) -> Result<Reply, String> {
    request(socket, Method::GET, path, HeaderMap::new(), Bytes::new()).await
}

async fn wait_healthy(socket: &Path) -> u64 {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(Ok(r)) =
            tokio::time::timeout(Duration::from_millis(500), get(socket, "/health")).await
            && r.status == StatusCode::OK
        {
            let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
            return v["pid"].as_u64().unwrap();
        }
        assert!(Instant::now() < deadline, "daemon did not answer /health");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_exit(child: &mut Child, within: Duration) -> std::process::ExitStatus {
    let deadline = Instant::now() + within;
    loop {
        if let Some(s) = child.try_wait().unwrap() {
            return s;
        }
        assert!(Instant::now() < deadline, "daemon did not exit");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// One HTTP/1.1 request on a raw stream; the whole answer as text. The
/// daemon closes the connection after it, as it is stopping.
async fn raw_get(stream: &mut tokio::net::UnixStream, path: &str) -> String {
    let req = format!("GET {path} HTTP/1.1\r\nhost: s1\r\n\r\n");
    let exchange = async {
        stream.write_all(req.as_bytes()).await.unwrap();
        let mut out = Vec::new();
        stream.read_to_end(&mut out).await.unwrap();
        out
    };
    let out = tokio::time::timeout(Duration::from_secs(10), exchange)
        .await
        .expect("no answer on the early connection");
    String::from_utf8(out).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_on_inherited_listeners_loses_no_request() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let gate = gated_mock().await;
    let cfg = root.join("config/s1");
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(cfg.join("models.toml"), overlay_for(gate.addr)).unwrap();
    // A config `listen.tcp` the daemon must not bind: it uses fd 4.
    std::fs::write(cfg.join("config.toml"), "[listen]\ntcp = \"127.0.0.1:1\"\n").unwrap();
    let run = root.join("run");
    std::fs::create_dir_all(&run).unwrap();
    std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o700)).unwrap();
    let socket = run.join("s1.sock");

    // The listeners systemd would own.
    let unix = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
    let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let tcp_addr = tcp.local_addr().unwrap();
    let socket_id = std::fs::metadata(&socket).unwrap();

    let mut first = spawn_activated(root, unix.as_raw_fd(), tcp.as_raw_fd());
    let first_pid = wait_healthy(&socket).await;
    assert_eq!(first_pid, u64::from(first.0.id()));
    let info: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("state/s1/daemon.json")).unwrap()).unwrap();
    assert_eq!(info["pid"].as_u64(), Some(first_pid));
    assert_eq!(info["tcp"].as_str(), Some(tcp_addr.to_string().as_str()));

    // The inherited TCP listener serves HTTP, bearer required.
    let token = s1::secrets::KeyResolver::new(root.join("home/.s1/auth.json"), |_| None)
        .ensure_local_token()
        .unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{tcp_addr}");
    let none = client.get(format!("{base}/health")).send().await.unwrap();
    assert_eq!(none.status(), StatusCode::UNAUTHORIZED);
    let health = client
        .get(format!("{base}/health"))
        .bearer_auth(token.expose())
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    let health: serde_json::Value = serde_json::from_slice(&health).unwrap();
    assert_eq!(health["pid"].as_u64(), Some(first_pid));
    assert_eq!(health["socket_activated"], true);

    // A request in its handler (over TCP), and a connection that has not
    // sent its request yet; then SIGTERM. Both must be answered.
    let open = {
        let (client, token) = (client.clone(), token.expose().to_string());
        let url = format!("{base}/v1/raw/systemone");
        // The status and the whole body, so the timeout below covers both.
        tokio::spawn(async move {
            let r = client
                .post(url)
                .bearer_auth(token)
                .header("content-type", "application/json")
                .body(fixture("typesafe/noul.request.json"))
                .send()
                .await?;
            Ok::<_, reqwest::Error>((r.status(), r.bytes().await?))
        })
    };
    tokio::time::timeout(Duration::from_secs(10), gate.entered.notified())
        .await
        .expect("request did not reach the provider");
    let mut early = tokio::net::UnixStream::connect(&socket).await.unwrap();
    // Let the daemon accept it (else it waits in the backlog for the next).
    tokio::time::sleep(Duration::from_millis(200)).await;
    // SAFETY: kill(2) on the child we spawned.
    assert_eq!(unsafe { libc::kill(first.0.id() as i32, libc::SIGTERM) }, 0);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let late = raw_get(&mut early, "/health").await;
    assert!(late.starts_with("HTTP/1.1 200"), "{late}");
    assert!(late.contains(&format!("\"pid\":{first_pid}")), "{late}");
    assert!(
        first.0.try_wait().unwrap().is_none(),
        "daemon exited with a request open"
    );
    gate.release.notify_one();
    let (status, body) = tokio::time::timeout(Duration::from_secs(10), open)
        .await
        .expect("open request was not answered")
        .unwrap()
        .expect("open request was dropped or cut on SIGTERM");
    assert_eq!(status, StatusCode::OK);
    let got: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let want: serde_json::Value =
        serde_json::from_slice(&fixture("typesafe/noul.response.json")).unwrap();
    assert_eq!(got, want);
    let status = wait_exit(&mut first.0, Duration::from_secs(15)).await;
    assert!(status.success(), "first daemon exit: {status:?}");

    // The socket file is systemd's: still there, same inode. No daemon runs
    // now; requests wait in the backlogs.
    let after = std::fs::metadata(&socket).expect("daemon removed the inherited socket");
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            (after.dev(), after.ino()),
            (socket_id.dev(), socket_id.ino())
        );
    }
    let pending = {
        let socket = socket.clone();
        tokio::spawn(async move { get(&socket, "/health").await })
    };
    let pending_tcp = {
        let req = client
            .get(format!("{base}/health"))
            .bearer_auth(token.expose());
        tokio::spawn(async move { req.send().await })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !pending.is_finished() && !pending_tcp.is_finished(),
        "request finished with no daemon running"
    );

    let mut second = spawn_activated(root, unix.as_raw_fd(), tcp.as_raw_fd());
    let reply = tokio::time::timeout(Duration::from_secs(20), pending)
        .await
        .expect("queued request was not answered")
        .unwrap()
        .expect("queued request failed");
    assert_eq!(reply.status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&reply.body).unwrap();
    assert_eq!(v["pid"].as_u64(), Some(u64::from(second.0.id())));
    let tcp_reply = tokio::time::timeout(Duration::from_secs(20), pending_tcp)
        .await
        .expect("queued TCP request was not answered")
        .unwrap()
        .expect("queued TCP request failed");
    assert_eq!(tcp_reply.status(), StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&tcp_reply.bytes().await.unwrap()).unwrap();
    assert_eq!(v["pid"].as_u64(), Some(u64::from(second.0.id())));

    // `s1 util daemon stop` does not fight systemd.
    let mut stop = Command::new(env!("CARGO_BIN_EXE_s1"));
    stop.args(["util", "daemon", "stop"]);
    s1_env(&mut stop, root);
    let out = tokio::task::spawn_blocking(move || stop.output().unwrap())
        .await
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("systemctl --user stop s1.socket"), "{err}");
    assert!(second.0.try_wait().unwrap().is_none(), "stop signalled it");

    // SAFETY: kill(2) on the child we spawned.
    unsafe { libc::kill(second.0.id() as i32, libc::SIGTERM) };
    let status = wait_exit(&mut second.0, Duration::from_secs(15)).await;
    assert!(status.success());
    assert!(socket.exists());
    assert!(
        !root.join("state/s1/daemon.json").exists(),
        "daemon.json of a stopped daemon is left behind"
    );
}

#[test]
fn listen_pid_of_another_process_is_ignored() {
    // `LISTEN_PID` names the parent shell, not s1: s1 binds as usual (and
    // does not take fds 3.. it was not given).
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let cfg = root.join("config/s1");
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(cfg.join("config.toml"), "[listen]\ntcp = \"127.0.0.1:0\"\n").unwrap();
    let mut child = Daemon::spawn(
        Command::new("sh")
            .args(["-c", "LISTEN_PID=$$ \"$0\" util daemon run; exit $?"])
            .arg(env!("CARGO_BIN_EXE_s1"))
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", root.join("home"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("XDG_RUNTIME_DIR", root.join("run"))
            .env("LISTEN_FDS", "2")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
    );
    let socket = root.join("run/s1.sock");
    let rt = tokio::runtime::Runtime::new().unwrap();
    let pid = rt.block_on(wait_healthy(&socket));
    assert_ne!(
        pid,
        u64::from(child.0.id()),
        "s1 must run as the shell's child"
    );
    // SAFETY: kill(2) on the daemon that reported its pid.
    unsafe { libc::kill(pid as i32, libc::SIGTERM) };
    let status = rt.block_on(wait_exit(&mut child.0, Duration::from_secs(15)));
    assert!(status.success());
    assert!(
        !socket.exists(),
        "a daemon that bound the socket removes it"
    );
}
