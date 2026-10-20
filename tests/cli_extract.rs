//! End-to-end `s1 extract` with a temp HOME and a mock Workers AI server.

mod common;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use common::{CF_ACCOUNT, CF_TOKEN, Mocks};
use serde_json::{Value, json};

struct Env {
    dir: tempfile::TempDir,
    _rt: tokio::runtime::Runtime,
    mocks: Mocks,
}

impl Env {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mocks = rt.block_on(Mocks::start());
        let env = Self {
            dir,
            _rt: rt,
            mocks,
        };
        let cfg = env.root().join("config/s1");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(cfg.join("models.toml"), env.mocks.overlay()).unwrap();
        std::fs::write(cfg.join("config.toml"), "[listen]\ntcp = \"127.0.0.1:0\"\n").unwrap();
        env.ok(
            &["auth", "set", "workers-ai", "--account-id", CF_ACCOUNT],
            CF_TOKEN.as_bytes(),
        );
        env
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn template(&self, name: &str, v: Value) -> PathBuf {
        let p = self.root().join(name);
        std::fs::write(&p, serde_json::to_vec(&v).unwrap()).unwrap();
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

    /// The `state` of the last request the mock got.
    fn last_state(&self) -> Value {
        let sent: Value = serde_json::from_slice(&self.mocks.clef.last().body).unwrap();
        assert_eq!(sent["model"], "clairic");
        sent["state"].clone()
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

fn lines(out: &Output) -> Vec<Value> {
    text(&out.stdout)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("{e}: {l}")))
        .collect()
}

fn invoice() -> Value {
    json!({
        "fields": {
            "invoice_number": {"type": "text"},
            "total": {"type": "number", "decimals": 2},
            "days_to_pay": {"type": "estimate", "min": 0, "max": 120, "step": 1}
        }
    })
}

#[test]
fn single_mode_from_an_argument_a_document_or_dash() {
    let env = Env::new();
    let t = env.template("invoice.clairic.json", invoice());
    let t = t.to_str().unwrap();

    let out = env.ok(&["extract", "-t", t, "INV-1 total 4"], b"");
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["model"], "clairic");
    assert_eq!(v["answers"]["invoice_number"]["value"], "INV-1");
    assert_eq!(env.last_state(), "INV-1 total 4");

    // A plain-text document on stdin is one state, unchanged.
    let doc = "INV-2291 from Northwind\nnet 30\n{not json}\n";
    let out = env.ok(&["extract", "-t", t, "--flat"], doc.as_bytes());
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(env.last_state(), doc);
    assert_eq!(
        v,
        json!({
            "invoice_number": "INV-2291",
            "invoice_number__conf": 0.99,
            "invoice_number__p_null": 0.01,
            "invoice_number__guess": "INV-2291",
            "invoice_number__span": [0, 8],
            "total": 42,
            "total__conf": 0.9,
            "total__p_null": 0.01,
            "total__guess": 42,
            "total__span": null,
            "days_to_pay": 30,
            "days_to_pay__median": 30,
            "days_to_pay__p10": 20,
            "days_to_pay__p90": 40
        })
    );

    // `-` reads all of stdin, even when it looks like JSON lines.
    let json_doc = "{\"invoice\": \"INV-3\"}\n";
    env.ok(&["extract", "-t", t, "-"], json_doc.as_bytes());
    assert_eq!(env.last_state(), json_doc);
}

#[test]
fn single_mode_errors_exit_non_zero() {
    let env = Env::new();
    let t = env.template("invoice.clairic.json", invoice());
    let out = env.run(
        &["extract", "-t", t.to_str().unwrap(), "-m", "decide", "x"],
        b"",
    );
    assert!(!out.status.success());
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["errors"][0]["code"], "wrong_family");

    let bad = env.template("bad.json", json!({"fields": {"is__bad": {"type": "text"}}}));
    let out = env.run(&["extract", "-t", bad.to_str().unwrap(), "x"], b"");
    assert!(!out.status.success());
    assert!(
        text(&out.stderr).contains("is__bad"),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn streaming_refuses_id_fields_that_collide_with_output_keys() {
    let env = Env::new();
    let mut tpl = invoice();
    tpl["fields"]["id"] = json!({"type": "text"});
    let with_id = env.template("with_id.clairic.json", tpl);
    let with_id = with_id.to_str().unwrap();
    let t = env.template("invoice.clairic.json", invoice());
    let t = t.to_str().unwrap();
    let line = b"{\"id\":\"a\",\"total__conf\":1,\"error\":2,\"response\":3,\"body\":\"x\"}\n";
    for args in [
        vec!["extract", "-t", with_id, "--flat"],
        vec!["extract", "-t", t, "--flat", "--id-field", "total__conf"],
        vec![
            "extract",
            "-t",
            t,
            "--flat",
            "--id-field",
            "days_to_pay__p90",
        ],
        vec!["extract", "-t", t, "--flat", "--id-field", "invoice_number"],
        vec!["extract", "-t", t, "--flat", "--id-field", "error"],
        vec!["extract", "-t", t, "--id-field", "error"],
        vec!["extract", "-t", t, "--id-field", "response"],
    ] {
        let out = env.run(&args, line);
        assert!(!out.status.success(), "{args:?} must fail");
        assert!(out.stdout.is_empty(), "{args:?}: {}", text(&out.stdout));
        let err = text(&out.stderr);
        assert!(
            err.contains("id field") || err.contains("--id-field"),
            "{args:?}: {err}"
        );
    }
    assert!(env.mocks.clef.all().is_empty(), "nothing was sent");
}

#[test]
fn streaming_image_field_sends_images_inline() {
    let env = Env::new();
    let t = env.template("invoice.clairic.json", invoice());
    let t = t.to_str().unwrap();
    let png = env.root().join("scan.png");
    std::fs::write(&png, b"\x89PNG\r\n\x1a\n0000").unwrap();
    let line = json!({"id": "a", "body": "", "scan": png.to_str().unwrap()}).to_string() + "\n";
    env.ok(
        &[
            "extract",
            "-t",
            t,
            "--state-field",
            "body",
            "--image-field",
            "scan",
        ],
        line.as_bytes(),
    );
    let sent: Value = serde_json::from_slice(&env.mocks.clef.all().last().unwrap().body).unwrap();
    assert_eq!(sent["state"], "");
    let images = sent["images"].as_array().unwrap();
    assert_eq!(images.len(), 1);
    assert!(
        images[0]
            .as_str()
            .unwrap()
            .starts_with("data:image/png;base64,"),
        "{sent}"
    );
}

#[test]
fn streaming_sends_non_string_states_as_json_text() {
    let env = Env::new();
    let t = env.template("invoice.clairic.json", invoice());
    let t = t.to_str().unwrap();
    let input = [
        json!({"id": "a", "body": "INV-10 paid", "customer": "c1"}),
        json!({"id": "b", "body": {"invoice": "INV-11", "total": 9}, "customer": "c2"}),
        json!({"id": "c", "body": "", "customer": "c3"}),
        json!({"id": "d", "body": "sleep:40 INV-13", "customer": "c4"}),
    ]
    .iter()
    .map(|v| v.to_string() + "\n")
    .collect::<String>();
    let out = env.run(
        &[
            "extract",
            "-t",
            t,
            "--state-field",
            "body",
            "--keep",
            "customer",
            "--flat",
        ],
        input.as_bytes(),
    );
    assert!(!out.status.success(), "one line failed: exit non-zero");
    assert!(
        text(&out.stderr).contains("1 of 4 lines failed"),
        "{}",
        text(&out.stderr)
    );
    let got = lines(&out);
    let ids: Vec<&str> = got.iter().map(|l| l["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["a", "b", "c", "d"], "input order");
    assert_eq!(got[0]["invoice_number"], "INV-10");
    assert_eq!(got[0]["days_to_pay__p90"], 40);
    assert_eq!(got[0]["customer"], "c1");
    // The object state went as JSON text; its first word is the answer.
    assert_eq!(
        got[1]["invoice_number"],
        "{\"invoice\":\"INV-11\",\"total\":9}"
    );
    // An empty state is refused by s1, as one error line.
    assert_eq!(got[2]["error"]["code"], "invalid_request");
    assert_eq!(got[2]["error"]["status"], 400);
    assert_eq!(got[2]["customer"], "c3");
    assert_eq!(got[3]["invoice_number"], "sleep:40");

    let states: Vec<Value> = env
        .mocks
        .clef
        .all()
        .iter()
        .map(|s| serde_json::from_slice::<Value>(&s.body).unwrap()["state"].clone())
        .collect();
    assert!(states.contains(&json!("{\"invoice\":\"INV-11\",\"total\":9}")));
    assert!(states.iter().all(Value::is_string));

    // Without --state-field the whole line is the state, as JSON text.
    let out = env.ok(&["extract", "-t", t], b"{\"id\":7,\"note\":\"INV-20\"}\n");
    let got = lines(&out);
    assert_eq!(got[0]["id"], 7);
    assert_eq!(got[0]["response"]["model"], "clairic");
    assert_eq!(env.last_state(), "{\"id\":7,\"note\":\"INV-20\"}");
}
