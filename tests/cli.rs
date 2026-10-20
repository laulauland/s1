//! End-to-end tests of the `s1` binary with a temp HOME and a mock TypeSafe.

mod common;

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use common::{TEST_KEY, fixture, overlay_for, start_mock};
use serde_json::Value;

struct Env {
    dir: tempfile::TempDir,
    _rt: tokio::runtime::Runtime,
    mock: common::Recorder,
}

impl Env {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (addr, mock) = rt.block_on(start_mock());
        let env = Self { dir, _rt: rt, mock };
        let cfg = env.root().join("config/s1");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(cfg.join("models.toml"), overlay_for(addr)).unwrap();
        std::fs::write(cfg.join("config.toml"), "[listen]\ntcp = \"127.0.0.1:0\"\n").unwrap();
        env
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }
    fn auth_file(&self) -> PathBuf {
        self.root().join("home/.s1/auth.json")
    }
    fn socket(&self) -> PathBuf {
        self.root().join("run/s1.sock")
    }
    fn state_dir(&self) -> PathBuf {
        self.root().join("state/s1")
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_s1"));
        c.args(args)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", self.root().join("home"))
            .env("XDG_CONFIG_HOME", self.root().join("config"))
            .env("XDG_STATE_HOME", self.root().join("state"))
            .env("XDG_CACHE_HOME", self.root().join("cache"))
            .env("XDG_RUNTIME_DIR", self.root().join("run"));
        c
    }

    fn run(&self, args: &[&str], stdin: &[u8]) -> Output {
        let mut child = self
            .cmd(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(stdin).unwrap();
        child.wait_with_output().unwrap()
    }

    fn ok(&self, args: &[&str], stdin: &[u8]) -> Output {
        let out = self.run(args, stdin);
        assert!(
            out.status.success(),
            "s1 {args:?} failed: {}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = self.run(&["daemon", "stop"], b"");
    }
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn mode(p: &Path) -> u32 {
    std::fs::metadata(p).unwrap().permissions().mode() & 0o777
}

#[test]
fn auth_token_is_made_once_with_private_modes() {
    let env = Env::new();
    let a = text(&env.ok(&["auth", "token"], b"").stdout);
    let b = text(&env.ok(&["auth", "token"], b"").stdout);
    assert_eq!(a, b);
    assert!(a.trim().starts_with("s1_"));
    assert_eq!(mode(&env.auth_file()), 0o600);
    assert_eq!(mode(env.auth_file().parent().unwrap()), 0o700);
}

#[test]
fn auth_set_reads_stdin_and_list_hides_values() {
    let env = Env::new();
    env.ok(
        &["auth", "set", "typesafe"],
        format!("{TEST_KEY}\n").as_bytes(),
    );
    env.ok(
        &["auth", "set", "workers-ai", "--account-id", "acc-1"],
        b"cf-secret-token",
    );
    let v: Value = serde_json::from_slice(&std::fs::read(env.auth_file()).unwrap()).unwrap();
    assert_eq!(
        v["typesafe"],
        serde_json::json!({"type": "api_key", "key": TEST_KEY})
    );
    assert_eq!(v["workers-ai"]["accountId"], "acc-1");

    let out = env.ok(&["auth", "list"], b"");
    let listed = text(&out.stdout) + &text(&out.stderr);
    assert!(listed.contains("typesafe"));
    assert!(listed.contains("workers-ai"));
    assert!(!listed.contains(TEST_KEY));
    assert!(!listed.contains("cf-secret-token"));
}

#[test]
fn auth_set_rejects_empty_key() {
    let env = Env::new();
    let out = env.run(&["auth", "set", "typesafe"], b"\n");
    assert!(!out.status.success());
}

#[test]
fn insecure_auth_file_is_refused() {
    let env = Env::new();
    env.ok(&["auth", "set", "typesafe"], TEST_KEY.as_bytes());
    std::fs::set_permissions(env.auth_file(), std::fs::Permissions::from_mode(0o644)).unwrap();
    let out = env.run(&["auth", "list"], b"");
    assert!(!out.status.success());
    assert!(text(&out.stderr).contains("chmod 600"));
    assert!(!text(&out.stderr).contains(TEST_KEY));
}

#[test]
fn daemon_lifecycle_models_raw_and_logs() {
    let env = Env::new();
    env.ok(&["auth", "set", "typesafe"], TEST_KEY.as_bytes());

    // `models` starts the daemon on first use.
    assert!(!env.socket().exists());
    let out = env.ok(&["models"], b"");
    let table = text(&out.stdout);
    assert!(env.socket().exists());
    assert_eq!(mode(&env.socket()), 0o600);
    let jev = table
        .lines()
        .find(|l| l.starts_with("typesafe/jev-latest "))
        .expect("jev listed");
    assert!(jev.ends_with("yes"), "{jev}");
    assert!(table.contains("openai/gpt-6-luna"));

    let json: Value = serde_json::from_slice(&env.ok(&["models", "--json"], b"").stdout).unwrap();
    assert!(json["data"].as_array().unwrap().len() >= 6);

    // `daemon start` sees the running daemon.
    let out = env.ok(&["daemon", "start"], b"");
    assert!(text(&out.stdout).contains("already running"));

    // `raw systemone` passes bytes both ways unchanged.
    let body = fixture("typesafe/noul.request.json");
    let out = env.ok(&["raw", "systemone", "-i"], &body);
    assert_eq!(out.stdout, fixture("typesafe/noul.response.json"));
    let headers = text(&out.stderr);
    assert!(headers.contains("HTTP 200"), "{headers}");
    assert!(headers.contains("s1-backend: typesafe"), "{headers}");
    assert!(
        headers.contains("s1-model: typesafe/jev-latest"),
        "{headers}"
    );
    assert_eq!(env.mock.last().body.as_ref(), body.as_slice());

    // TCP listener with the bearer token from `auth token`.
    let token = text(&env.ok(&["auth", "token"], b"").stdout)
        .trim()
        .to_string();
    let info: Value =
        serde_json::from_slice(&std::fs::read(env.state_dir().join("daemon.json")).unwrap())
            .unwrap();
    let tcp = info["tcp"].as_str().unwrap().to_string();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (no_token, with_token) = rt.block_on(async {
        let c = reqwest::Client::new();
        let url = format!("http://{tcp}/v1/models");
        let a = c.get(&url).send().await.unwrap().status().as_u16();
        let b = c
            .get(&url)
            .bearer_auth(&token)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16();
        (a, b)
    });
    assert_eq!((no_token, with_token), (401, 200));

    // A provider error is returned unchanged with a failing exit code.
    std::fs::write(
        env.auth_file(),
        r#"{"typesafe":{"type":"api_key","key":"wrong"}}"#,
    )
    .unwrap();
    std::fs::set_permissions(env.auth_file(), std::fs::Permissions::from_mode(0o600)).unwrap();
    let out = env.run(&["raw", "systemone"], &body);
    assert!(!out.status.success());
    assert_eq!(out.stdout, fixture("typesafe/invalid_key.401.json"));

    // JSON logs with metadata, no payloads or keys.
    let out = env.ok(&["daemon", "logs", "-n", "100"], b"");
    let logs = text(&out.stdout);
    let lines: Vec<Value> = logs
        .lines()
        .map(|l| serde_json::from_str(l).expect("JSON log line"))
        .collect();
    assert!(lines.iter().any(|l| l["fields"]["backend"] == "typesafe"
        && l["fields"]["model"] == "typesafe/jev-latest"
        && l["fields"]["status"] == 200));
    assert!(!logs.contains("charged twice"), "no payloads in logs");
    assert!(!logs.contains(TEST_KEY), "no keys in logs");
    assert!(!logs.contains(&token), "no token in logs");
    let log_files: Vec<_> = std::fs::read_dir(env.state_dir())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("s1.log"))
        .collect();
    assert_eq!(log_files.len(), 1);

    // Stop.
    let out = env.ok(&["daemon", "stop"], b"");
    assert!(text(&out.stdout).contains("stopped"));
    assert!(!env.socket().exists());
    assert!(!env.state_dir().join("daemon.json").exists());
    let out = env.ok(&["daemon", "stop"], b"");
    assert!(text(&out.stdout).contains("not running"));
}

#[test]
fn raw_auto_starts_daemon() {
    let env = Env::new();
    env.ok(&["auth", "set", "typesafe"], TEST_KEY.as_bytes());
    let out = env.ok(
        &["raw", "systemone"],
        &fixture("typesafe/noul.request.json"),
    );
    assert_eq!(out.stdout, fixture("typesafe/noul.response.json"));
}

#[test]
fn bad_overlay_reports_daemon_start_error() {
    let env = Env::new();
    std::fs::write(
        env.root().join("config/s1/models.toml"),
        "[providers.typesafe",
    )
    .unwrap();
    let out = env.run(&["models"], b"");
    assert!(!out.status.success());
    assert!(
        text(&out.stderr).contains("models.toml"),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn daemon_stop_signals_the_daemon_not_a_stale_pid() {
    let env = Env::new();
    env.ok(&["daemon", "start"], b"");
    // Stale metadata naming an unrelated live process.
    let mut bystander = Command::new("sleep").arg("30").spawn().unwrap();
    let file = env.state_dir().join("daemon.json");
    let mut info: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    info["pid"] = bystander.id().into();
    std::fs::write(&file, serde_json::to_vec(&info).unwrap()).unwrap();

    let out = env.run(&["daemon", "stop"], b"");
    let alive = bystander.try_wait().unwrap().is_none();
    let _ = bystander.kill();
    let _ = bystander.wait();
    assert!(alive, "the unrelated process was signalled");
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(!env.socket().exists());
}

/// Without `XDG_RUNTIME_DIR` the socket falls back to the state dir. Under
/// umask 000 the CLI must still make that dir private before the daemon
/// publishes or the CLI trusts anything in it.
#[test]
fn fallback_socket_dir_is_private_under_permissive_umask() {
    use std::os::unix::process::CommandExt;
    let env = Env::new();
    let fallback = |args: &[&str]| {
        let mut c = env.cmd(args);
        c.env_remove("XDG_RUNTIME_DIR");
        // SAFETY: umask(2) is async-signal-safe.
        unsafe {
            c.pre_exec(|| {
                libc::umask(0);
                Ok(())
            });
        }
        c.output().unwrap()
    };
    let out = fallback(&["daemon", "start"]);
    let socket = env.state_dir().join("s1.sock");
    let modes = (
        mode(&env.state_dir()),
        socket.exists().then(|| mode(&socket)),
    );
    let stop = fallback(&["daemon", "stop"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(stop.status.success(), "{}", text(&stop.stderr));
    assert_eq!(modes, (0o700, Some(0o600)));
}

/// An existing socket dir that others can access is made private (this user
/// owns it) before the daemon binds in it.
#[test]
fn existing_open_socket_dir_is_made_private() {
    let env = Env::new();
    let run = env.root().join("run");
    std::fs::create_dir_all(&run).unwrap();
    std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o777)).unwrap();
    env.ok(&["daemon", "start"], b"");
    assert_eq!(mode(&run), 0o700);
    assert_eq!(mode(&env.socket()), 0o600);
}

/// The CLI does not trust a socket path that is a symlink (it could point at
/// a listener that another user controls), and the daemon does not bind over it.
#[test]
fn symlinked_socket_is_not_trusted() {
    let env = Env::new();
    let other = Env::new();
    other.ok(&["daemon", "start"], b"");
    let run = env.root().join("run");
    std::fs::create_dir_all(&run).unwrap();
    std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::os::unix::fs::symlink(other.socket(), env.socket()).unwrap();
    let out = env.run(&["models"], b"");
    assert!(!out.status.success(), "{}", text(&out.stdout));
    assert!(
        text(&out.stderr).contains("symlink"),
        "{}",
        text(&out.stderr)
    );
    let start = env.run(&["daemon", "run"], b"");
    assert!(!start.status.success());
    assert!(
        text(&start.stderr).contains("symlink"),
        "{}",
        text(&start.stderr)
    );
}
