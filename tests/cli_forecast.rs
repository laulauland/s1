//! End-to-end `s1 forecast --csv` with a temp HOME and a mock TSFM server
//! registered through `models.toml`.

mod tsfm_mock;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::{Value, json};
use tsfm_mock::{Recorder, TSFM_KEY};

struct Env {
    dir: tempfile::TempDir,
    _rt: tokio::runtime::Runtime,
    mock: Recorder,
}

impl Env {
    /// `with_backend`: write the overlay and key for the mock.
    fn new(with_backend: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (addr, mock) = rt.block_on(tsfm_mock::start());
        let env = Self { dir, _rt: rt, mock };
        let cfg = env.root().join("config/s1");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(cfg.join("config.toml"), "[listen]\ntcp = \"127.0.0.1:0\"\n").unwrap();
        if with_backend {
            std::fs::write(cfg.join("models.toml"), tsfm_mock::overlay(addr)).unwrap();
            env.ok(&["auth", "set", "tsfm"], TSFM_KEY.as_bytes());
        }
        env
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn file(&self, name: &str, text: &str) -> PathBuf {
        let p = self.root().join(name);
        std::fs::write(&p, text).unwrap();
        p
    }

    fn run(&self, args: &[&str], stdin: &[u8]) -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_s1"))
            .args(args)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", self.root().join("home"))
            .env("XDG_CONFIG_HOME", self.root().join("config"))
            .env("XDG_STATE_HOME", self.root().join("state"))
            .env("XDG_CACHE_HOME", self.root().join("cache"))
            .env("XDG_RUNTIME_DIR", self.root().join("run"))
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
            text(&out.stdout),
            text(&out.stderr)
        );
        out
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = self.run(&["util", "daemon", "stop"], b"");
    }
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

const SALES: &str = "sku,ds,y\n\
                     a,2026-02-01,10\n\
                     a,2026-02-02,11\n\
                     b,2026-02-01,5\n\
                     a,2026-02-04,13\n\
                     b,2026-02-03,7\n";

#[test]
fn csv_helper_builds_a_regular_grid_and_prints_long_rows() {
    let env = Env::new(true);
    let csv = env.file("sales.csv", SALES);
    let csv = csv.to_str().unwrap();
    let args = [
        "forecast", "-m", "forecast", "--csv", csv, "--id", "sku", "--time", "ds", "--value", "y",
        "-h", "2", "-q", "0.2,0.8", "--flat",
    ];
    let out = env.ok(&args, b"");
    let rows: Vec<Value> = text(&out.stdout)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    // Two series, two steps each.
    assert_eq!(rows.len(), 4);
    assert_eq!(
        rows[0],
        json!({"id": "a", "step": 1, "timestamp": "2026-02-05T00:00:00Z",
               "mean": 13.0, "q0.1": 9.0, "q0.5": 13.0, "q0.9": 17.0})
    );
    assert_eq!(rows[2]["id"], "b");
    assert_eq!(rows[2]["timestamp"], "2026-02-04T00:00:00Z");

    let sent: Value = serde_json::from_slice(&env.mock.last().body).unwrap();
    assert_eq!(sent["model"], "amazon/chronos-bolt-small");
    assert_eq!(
        sent["inputs"],
        json!([
            {"start": "2026-02-01T00:00:00Z", "target": [[10.0], [11.0], [null], [13.0]], "metadata": {"id": "a"}},
            {"start": "2026-02-01T00:00:00Z", "target": [[5.0], [null], [7.0]], "metadata": {"id": "b"}}
        ])
    );
    assert_eq!(
        sent["parameters"],
        json!({"prediction_length": 2, "frequency": "D", "quantile_levels": [0.2, 0.8]})
    );

    // Without --flat: the TSFM response, levels as produced.
    let out = env.ok(&args[..args.len() - 1], b"");
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["quantile_levels"], json!([0.1, 0.5, 0.9]));
}

#[test]
fn print_request_needs_no_daemon_and_csv_can_come_from_stdin() {
    let env = Env::new(false);
    let out = env.ok(
        &[
            "forecast",
            "--csv",
            "-",
            "--id",
            "sku",
            "--time",
            "ds",
            "--value",
            "y",
            "-h",
            "28",
            "--print-request",
        ],
        SALES.as_bytes(),
    );
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["model"], "forecast");
    assert_eq!(v["parameters"]["prediction_length"], 28);
    assert_eq!(v["inputs"][1]["target"], json!([[5.0], [null], [7.0]]));
}

#[test]
fn irregular_csv_stops_with_a_clear_error() {
    let env = Env::new(false);
    let csv = env.file(
        "bad.csv",
        "ds,y\n2026-01-01,1\n2026-01-03,2\n2026-01-06,3\n",
    );
    let out = env.run(
        &[
            "forecast",
            "--csv",
            csv.to_str().unwrap(),
            "--time",
            "ds",
            "--value",
            "y",
        ],
        b"",
    );
    assert!(!out.status.success());
    let err = text(&out.stderr);
    assert!(err.contains("irregular") && err.contains("--freq"), "{err}");
}

#[test]
fn without_a_backend_the_cli_shows_backend_unavailable() {
    let env = Env::new(false);
    let csv = env.file("sales.csv", SALES);
    let out = env.run(
        &[
            "forecast",
            "--csv",
            csv.to_str().unwrap(),
            "--id",
            "sku",
            "--time",
            "ds",
            "--value",
            "y",
            "-h",
            "3",
        ],
        b"",
    );
    assert!(!out.status.success());
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["code"], "backend_unavailable");
    assert_eq!(v["endpoint"], "/v1/forecast");
}

#[test]
fn stdin_tsfm_request_is_sent_as_is() {
    let env = Env::new(true);
    let req = std::fs::read(format!(
        "{}/tests/fixtures/tsfm/docs.request.json",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    let out = env.ok(&["forecast", "-m", "forecast", "--flat"], &req);
    let rows: Vec<Value> = text(&out.stdout)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(rows.len(), 7);
    assert_eq!(rows[0]["id"], "store_017");
    assert_eq!(rows[0]["q0.9"], 485.5);
}

#[test]
fn stdin_request_takes_horizon_freq_and_quantile_flags() {
    let env = Env::new(false);
    let req = std::fs::read(format!(
        "{}/tests/fixtures/tsfm/docs.request.json",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    let out = env.ok(
        &[
            "forecast",
            "-h",
            "3",
            "--freq",
            "2D",
            "-q",
            "0.25,0.75",
            "--print-request",
        ],
        &req,
    );
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["parameters"]["prediction_length"], 3);
    assert_eq!(v["parameters"]["frequency"], "2D");
    assert_eq!(v["parameters"]["quantile_levels"], json!([0.25, 0.75]));
}

#[test]
fn non_finite_csv_values_stop_with_an_error() {
    let env = Env::new(false);
    let csv = env.file("inf.csv", "ds,y\n2026-01-01,1\n2026-01-02,inf\n");
    let out = env.run(
        &[
            "forecast",
            "--csv",
            csv.to_str().unwrap(),
            "--time",
            "ds",
            "--value",
            "y",
            "--print-request",
        ],
        b"",
    );
    assert!(!out.status.success());
    let err = text(&out.stderr);
    assert!(err.contains("line 3") && err.contains("finite"), "{err}");
}
