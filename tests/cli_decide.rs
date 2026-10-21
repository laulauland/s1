//! End-to-end `s1 decide` with a temp HOME and mock Jev and Clef servers.

mod common;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use common::{CF_ACCOUNT, CF_TOKEN, Mocks, TEST_KEY};
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
        env.ok(&["auth", "set", "typesafe"], TEST_KEY.as_bytes());
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
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = self.run(&["util", "daemon", "stop"], b"");
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

fn triage() -> Value {
    json!({
        "questions": {
            "urgent": {"type": "noul", "instructions": "Is this urgent?"},
            "topic": {"type": "choice", "instructions": "Topic?", "criteria": {"billing": "Money", "tech": "Bugs"}}
        }
    })
}

#[test]
fn single_mode_prints_the_provider_response() {
    let env = Env::new();
    let t = env.template("triage.json", triage());
    let t = t.to_str().unwrap();
    let out = env.ok(&["decide", "-t", t, "Checkout is down, yes"], b"");
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    // Default alias `decide` -> clef-flash.
    assert_eq!(v["model"], "clef-flash");
    assert_eq!(v["answers"]["urgent"]["noul"], 0.9);
    let sent: Value = serde_json::from_slice(&env.mocks.clef.last().body).unwrap();
    assert_eq!(sent["state"], "Checkout is down, yes");

    let out = env.ok(
        &["decide", "-t", t, "-m", "jev-latest", "--flat", "no"],
        b"",
    );
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        v,
        json!({"urgent": 0.1, "topic": "billing", "topic__conf": 0.6, "topic__p__billing": 0.7, "topic__p__tech": 0.3})
    );
    assert!(!env.mocks.jev.all().is_empty());
}

#[test]
fn single_mode_errors_exit_non_zero() {
    let env = Env::new();
    let t = env.template("triage.json", triage());
    let out = env.run(
        &[
            "decide",
            "-t",
            t.to_str().unwrap(),
            "-m",
            "workers-ai/clairic",
            "x",
        ],
        b"",
    );
    assert!(!out.status.success());
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["detail"]["error_type"], "wrong_family");
}

#[test]
fn templates_with_double_underscore_names_are_rejected() {
    let env = Env::new();
    let t = env.template(
        "bad.json",
        json!({"questions": {"is__bad": {"type": "noul", "instructions": "x"}}}),
    );
    let out = env.run(&["decide", "-t", t.to_str().unwrap(), "x"], b"");
    assert!(!out.status.success());
    assert!(
        text(&out.stderr).contains("is__bad"),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn streaming_one_line_in_one_line_out_with_errors_per_line() {
    let env = Env::new();
    let t = env.template("triage.json", triage());
    let input = [
        json!({"id": "t1", "body": "refund please, yes", "customer": "c1"}),
        json!({"id": "t2", "customer": "c2"}),
        json!({"id": "t3", "body": "sleep:50 slow one", "customer": "c3"}),
        json!({"id": "t4", "body": "fast one", "customer": "c4"}),
    ]
    .iter()
    .map(|v| v.to_string() + "\n")
    .collect::<String>();
    let out = env.run(
        &[
            "decide",
            "-t",
            t.to_str().unwrap(),
            "--state-field",
            "body",
            "--keep",
            "customer",
            "--flat",
        ],
        input.as_bytes(),
    );
    assert!(!out.status.success(), "one line failed: exit non-zero");
    assert!(text(&out.stderr).contains("1 of 4 lines failed"));
    let got = lines(&out);
    let ids: Vec<&str> = got.iter().map(|l| l["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["t1", "t2", "t3", "t4"], "input order");
    assert_eq!(got[0]["urgent"], 0.9);
    assert_eq!(got[0]["customer"], "c1");
    assert_eq!(got[0]["topic__p__billing"], 0.7);
    assert!(got[0].get("body").is_none(), "not kept");
    assert_eq!(got[1]["customer"], "c2");
    assert_eq!(got[1]["error"]["message"], "input line has no `body` field");
    assert_eq!(got[2]["urgent"], 0.1);
}

#[test]
fn streaming_unordered_and_custom_id_field() {
    let env = Env::new();
    let t = env.template("triage.json", triage());
    let input = format!(
        "{}\n{}\n",
        json!({"key": 1, "body": "sleep:300 slow"}),
        json!({"key": 2, "body": "fast"})
    );
    let out = env.ok(
        &[
            "decide",
            "-t",
            t.to_str().unwrap(),
            "--jsonl",
            "--id-field",
            "key",
            "--state-field",
            "body",
            "--unordered",
        ],
        input.as_bytes(),
    );
    let got = lines(&out);
    let ids: Vec<i64> = got.iter().map(|l| l["key"].as_i64().unwrap()).collect();
    assert_eq!(ids, [2, 1], "completion order");
    assert_eq!(got[0]["response"]["model"], "clef-flash");
}

#[test]
fn cascade_with_keep_star_and_collision_check() {
    let env = Env::new();
    let step1 = env.template("topic.json", triage());
    let step2 = env.template(
        "billing.json",
        json!({"model": "jev-latest", "questions": {"threatens_cancel": {"type": "noul", "instructions": "Cancel?"}}}),
    );
    let input = format!("{}\n", json!({"id": 7, "body": "I will cancel, yes"}));
    let common = ["--state-field", "body", "--keep", "*", "--flat"];
    let mut a = vec!["decide", "-t", step1.to_str().unwrap()];
    a.extend(common);
    let out1 = env.ok(&a, input.as_bytes());
    let mut b = vec!["decide", "-t", step2.to_str().unwrap()];
    b.extend(common);
    let out2 = env.ok(&b, &out1.stdout);
    let got = lines(&out2);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0]["id"], 7);
    assert_eq!(got[0]["body"], "I will cancel, yes");
    assert_eq!(got[0]["topic"], "billing");
    assert_eq!(got[0]["threatens_cancel"], 0.9);

    // Running step 1 again on its own output collides with its columns.
    let out = env.run(&a, &out1.stdout);
    assert!(!out.status.success());
    let err = text(&out.stderr);
    assert!(err.contains("same name as a new answer column"), "{err}");
    assert!(out.stdout.is_empty(), "nothing overwritten");
}

#[test]
fn image_field_sends_paths_that_the_daemon_reads() {
    let env = Env::new();
    let t = env.template("triage.json", triage());
    let png = env.root().join("door.png");
    std::fs::write(&png, b"\x89PNG\r\n\x1a\nrest").unwrap();
    let input = [
        json!({"id": "a", "body": "yes", "photo": png}),
        json!({"id": "b", "body": "yes", "photo": env.root().join("missing.png")}),
    ]
    .iter()
    .map(|v| v.to_string() + "\n")
    .collect::<String>();
    let out = env.run(
        &[
            "decide",
            "-t",
            t.to_str().unwrap(),
            "--state-field",
            "body",
            "--image-field",
            "photo",
            "-j",
            "1",
        ],
        input.as_bytes(),
    );
    let got = lines(&out);
    assert!(got[0].get("error").is_none(), "{}", got[0]);
    assert_eq!(got[1]["error"]["code"], "invalid_request", "{}", got[1]);
    assert!(
        got[1]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("cannot read image")
    );
    let sent: Value = serde_json::from_slice(&env.mocks.jev.last().body).unwrap();
    let asset = sent["assets"].as_object().unwrap().values().next().unwrap();
    assert_eq!(asset["format"], "png");
    assert_eq!(asset["data_base64"], "iVBORw0KGgpyZXN0");
    assert!(asset.get("s1_path").is_none());
}

#[test]
fn cache_status_and_cache_clear_commands() {
    let env = Env::new();
    let t = env.template("triage.json", triage());
    let t = t.to_str().unwrap();
    let first = env.ok(&["decide", "-t", t, "Checkout is down, yes"], b"");
    let calls = env.mocks.clef.all().len();
    let second = env.ok(&["decide", "-t", t, "Checkout is down, yes"], b"");
    assert_eq!(first.stdout, second.stdout);
    assert_eq!(
        env.mocks.clef.all().len(),
        calls,
        "second answer from the cache"
    );
    env.ok(
        &["decide", "-t", t, "--no-cache", "Checkout is down, yes"],
        b"",
    );
    assert_eq!(
        env.mocks.clef.all().len(),
        calls + 1,
        "--no-cache asks again"
    );

    let out = env.ok(&["status", "--json"], b"");
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    let providers: Vec<&str> = v["backends"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["provider"].as_str().unwrap())
        .collect();
    assert!(
        providers.contains(&"typesafe") && providers.contains(&"workers-ai"),
        "{v}"
    );
    assert_eq!(v["cache"]["entries"], 1, "{v}");
    let out = env.ok(&["status"], b"");
    let table = text(&out.stdout);
    assert!(
        table.contains("BREAKER") && table.contains("workers-ai"),
        "{table}"
    );
    assert!(!table.contains(CF_TOKEN) && !table.contains(TEST_KEY));

    let out = env.ok(&["util", "cache", "clear"], b"");
    assert!(
        text(&out.stdout).contains("cleared 1"),
        "{}",
        text(&out.stdout)
    );
    env.ok(&["decide", "-t", t, "Checkout is down, yes"], b"");
    assert_eq!(env.mocks.clef.all().len(), calls + 2, "cleared: asks again");

    // Without a daemon the file is removed.
    env.ok(&["util", "daemon", "stop"], b"");
    let file = env.root().join("cache/s1/cache.redb");
    assert!(file.exists());
    let out = env.ok(&["util", "cache", "clear"], b"");
    assert!(
        text(&out.stdout).contains("removed"),
        "{}",
        text(&out.stdout)
    );
    assert!(!file.exists());
}

fn openai_triage() -> Value {
    json!({
        "questions": [
            {"type": "predicate", "name": "urgent", "instructions": "Is this urgent?"},
            {"type": "choice", "instructions": "Topic?", "choices": [
                {"value": "billing", "description": "Money"},
                {"value": true}
            ]}
        ]
    })
}

#[test]
fn openai_flag_uses_the_decisions_format() {
    let env = Env::new();
    env.ok(&["auth", "set", "openai"], common::OPENAI_KEY.as_bytes());
    let t = env.template("openai.json", openai_triage());
    let t = t.to_str().unwrap();
    // Default alias: Clef, translated both ways.
    let out = env.ok(
        &["decide", "--openai", "-t", t, "Checkout is down, yes"],
        b"",
    );
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["answers"][0]["name"], "urgent");
    assert_eq!(v["answers"][0]["probability"], 0.9);
    assert_eq!(v["answers"][1]["name"], Value::Null);
    assert_eq!(v["answers"][1]["choice"], "billing");
    let sent: Value = serde_json::from_slice(&env.mocks.clef.last().body).unwrap();
    assert_eq!(sent["state"], "Checkout is down, yes");

    // Native OpenAI, flat, streaming: unnamed questions are `q{i}`.
    let out = env.ok(
        &[
            "decide",
            "--openai",
            "-t",
            t,
            "-m",
            "openai/gpt-6-luna",
            "--flat",
            "--state-field",
            "body",
        ],
        b"{\"id\":7,\"body\":\"no\"}\n",
    );
    let rows = lines(&out);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], 7);
    assert_eq!(rows[0]["urgent"], 0.1);
    assert_eq!(rows[0]["q1"], "billing");
    assert_eq!(rows[0]["q1__p__bool:true"], 0.3);
    let sent: Value = serde_json::from_slice(&env.mocks.luna.last().body).unwrap();
    assert_eq!(sent["input"], "no");
    assert_eq!(sent["model"], "gpt-6-luna");
}
