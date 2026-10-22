//! Hot reload of `models.toml`: `POST /reload`, the file watcher, bad files,
//! and SIGHUP on the real daemon.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use bytes::Bytes;
use common::{Recorder, TEST_KEY, fixture, overlay_for, start_mock};
use s1::catalog::Catalog;
use s1::cli::client::{Reply, request};
use s1::daemon::{AppState, RunningServer, ServerOptions, serve};
use s1::paths::Paths;
use s1::secrets::{KeyResolver, Secret};
use serde_json::Value;

struct Harness {
    _dir: tempfile::TempDir,
    paths: Paths,
    server: RunningServer,
    token: Secret,
    mock_addr: std::net::SocketAddr,
    mock: Recorder,
}

impl Harness {
    /// `poll`: how often the daemon checks the file (`None`: only on
    /// request, so a test sees each reload it asks for).
    async fn new(poll: Option<Duration>) -> Self {
        Self::new_with(poll, false).await
    }

    /// `cache`: with an answer cache in the temp dir.
    async fn new_with(poll: Option<Duration>, cache: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        let keys = KeyResolver::new(paths.auth_file(), |_| None);
        keys.update(|f| f.set_api_key("typesafe", &Secret::new(TEST_KEY), None))
            .unwrap();
        let token = keys.ensure_local_token().unwrap();
        let (mock_addr, mock) = start_mock().await;
        std::fs::write(paths.models_file(), overlay_for(mock_addr)).unwrap();
        let catalog = Catalog::load(&paths.models_file()).unwrap();
        let mut state = AppState::new(catalog, keys)
            .with_models_file(paths.models_file())
            .with_models_poll(poll);
        if cache {
            let c = s1::cache::Cache::open(&dir.path().join("cache.redb"), 1 << 26).unwrap();
            state = state.with_cache(std::sync::Arc::new(c));
        }
        let server = serve(ServerOptions {
            socket: paths.socket.clone(),
            tcp: "127.0.0.1:0".parse().unwrap(),
            state,
            token: token.clone(),
        })
        .await
        .unwrap();
        Self {
            _dir: dir,
            paths,
            server,
            token,
            mock_addr,
            mock,
        }
    }

    /// The mock overlay plus a provider `extra` with model `m1` (TypeSafe
    /// format, the typesafe key).
    fn with_extra(&self) -> String {
        format!(
            "{}\n[providers.extra]\nbase_url = \"http://{}\"\napi = \"systemone\"\nauth = \"typesafe\"\n[[providers.extra.models]]\nid = \"m1\"\nupstream = \"jev-latest\"\n",
            overlay_for(self.mock_addr),
            self.mock_addr
        )
    }

    fn write(&self, text: &str) {
        // Replace, as editors do: write a temp file, then rename it over.
        let tmp = self.paths.config_dir.join(".models.toml.tmp");
        std::fs::write(&tmp, text).unwrap();
        std::fs::rename(&tmp, self.paths.models_file()).unwrap();
    }

    async fn call(&self, method: Method, path: &str, body: &[u8]) -> Reply {
        let mut h = HeaderMap::new();
        h.insert("content-type", HeaderValue::from_static("application/json"));
        request(
            &self.paths.socket,
            method,
            path,
            h,
            Bytes::copy_from_slice(body),
        )
        .await
        .unwrap()
    }

    async fn json(&self, method: Method, path: &str) -> (StatusCode, Value) {
        let r = self.call(method, path, b"").await;
        (r.status, serde_json::from_slice(&r.body).unwrap())
    }

    async fn has_model(&self, id: &str) -> bool {
        let r = self.call(Method::GET, "/v1/models", b"").await;
        String::from_utf8_lossy(&r.body).contains(&format!("\"{id}\""))
    }
}

fn raw_body(model: &str) -> Vec<u8> {
    let mut v: Value = serde_json::from_slice(&fixture("typesafe/noul.request.json")).unwrap();
    v["model"] = Value::String(model.into());
    serde_json::to_vec(&v).unwrap()
}

#[tokio::test]
async fn reload_endpoint_swaps_in_a_new_provider() {
    let h = Harness::new(None).await;
    assert!(!h.has_model("extra/m1").await);
    let before = h.mock.all().len();

    h.write(&h.with_extra());
    let (status, body) = h.json(Method::POST, "/reload").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["changed"], true);
    assert_eq!(body["added"], serde_json::json!(["extra/m1"]));
    assert_eq!(body["removed"], serde_json::json!([]));
    assert!(h.has_model("extra/m1").await);

    // The new provider has a working backend.
    let r = h
        .call(Method::POST, "/v1/systemone", &raw_body("extra/m1"))
        .await;
    assert_eq!(
        r.status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&r.body)
    );
    assert_eq!(r.headers["s1-model"], "extra/m1");
    assert_eq!(h.mock.all().len(), before + 1);

    // The same file again changes nothing.
    let (status, body) = h.json(Method::POST, "/reload").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["changed"], false);
    let (_, st) = h.json(Method::GET, "/status").await;
    assert_eq!(st["catalog"]["reloads"], 1);
    assert_eq!(
        st["catalog"]["file"].as_str().map(PathBuf::from),
        Some(h.paths.models_file())
    );
}

#[tokio::test]
async fn bad_file_keeps_the_old_catalog_and_reports_the_error() {
    let h = Harness::new(None).await;
    h.write(&h.with_extra());
    assert_eq!(h.json(Method::POST, "/reload").await.0, StatusCode::OK);

    for bad in [
        "[providers.extra\nbroken toml",
        "[aliases.decide]\nmodels = [\"nope/missing\"]\n",
        "[limits.typesafe]\nnot_a_limit = 1\n",
    ] {
        h.write(bad);
        let (status, body) = h.json(Method::POST, "/reload").await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}: {body}");
        assert_eq!(body["error"]["code"], "invalid_catalog");
        assert!(
            h.has_model("extra/m1").await,
            "old catalog lost after {bad:?}"
        );
        let (_, st) = h.json(Method::GET, "/status").await;
        assert!(st["catalog"]["last_error"].is_string(), "{st}");
        assert!(st["catalog"]["last_error_at"].is_u64());
    }

    // A good file clears the error.
    h.write(&overlay_for(h.mock_addr));
    let (status, body) = h.json(Method::POST, "/reload").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["removed"], serde_json::json!(["extra/m1"]));
    let (_, st) = h.json(Method::GET, "/status").await;
    assert!(st["catalog"]["last_error"].is_null());
    assert!(!h.has_model("extra/m1").await);
}

#[tokio::test]
async fn reload_is_refused_on_tcp() {
    let h = Harness::new(None).await;
    let r = reqwest::Client::new()
        .post(format!("http://{}/reload", h.server.tcp))
        .bearer_auth(h.token.expose())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 403);
}

#[tokio::test]
async fn fixed_catalog_has_nothing_to_reload() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    let keys = KeyResolver::new(paths.auth_file(), |_| None);
    let server = serve(ServerOptions {
        socket: paths.socket.clone(),
        tcp: "127.0.0.1:0".parse().unwrap(),
        state: AppState::new(Catalog::builtin(), keys),
        token: Secret::new("t"),
    })
    .await
    .unwrap();
    assert!(server.reload().is_err());
    assert!(server.catalog_status().file.is_none());
}

#[tokio::test]
async fn a_model_pointed_elsewhere_does_not_serve_old_cached_answers() {
    let h = Harness::new_with(None, true).await;
    h.write(&h.with_extra());
    let (status, _) = h.json(Method::POST, "/reload").await;
    assert_eq!(status, StatusCode::OK);
    let ask = async || {
        let r = h
            .call(Method::POST, "/v1/systemone", &raw_body("extra/m1"))
            .await;
        assert_eq!(
            r.status,
            StatusCode::OK,
            "{}",
            String::from_utf8_lossy(&r.body)
        );
        r
    };
    ask().await;
    let calls = h.mock.all().len();
    let hit = ask().await;
    assert_eq!(h.mock.all().len(), calls, "second ask is a cache hit");
    assert_eq!(hit.headers["s1-cache"], "hit");

    // Same id, another upstream: the provider is asked again.
    h.write(
        &h.with_extra()
            .replace("upstream = \"jev-latest\"", "upstream = \"jev-preview\""),
    );
    let (status, body) = h.json(Method::POST, "/reload").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["changed"], true);
    ask().await;
    assert_eq!(h.mock.all().len(), calls + 1, "old target's answer served");
    let sent: Value = serde_json::from_slice(&h.mock.last().body).unwrap();
    assert_eq!(sent["model"], "jev-preview");
}

async fn eventually(what: &str, mut check: impl AsyncFnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !check().await {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn watcher_reloads_on_edit_replace_and_delete() {
    let h = Harness::new(Some(Duration::from_millis(100))).await;

    // Edit in place.
    std::fs::write(h.paths.models_file(), h.with_extra()).unwrap();
    eventually("the edit", async || h.has_model("extra/m1").await).await;

    // A bad save is logged and kept for /status; the catalog stays.
    h.write("[[[");
    eventually("the error", async || {
        h.json(Method::GET, "/status").await.1["catalog"]["last_error"].is_string()
    })
    .await;
    assert!(h.has_model("extra/m1").await);

    // Deleting the overlay leaves the compiled-in catalog: no extra/m1, and
    // typesafe back at its public URL.
    h.write(&h.with_extra());
    eventually("the fix", async || {
        h.json(Method::GET, "/status").await.1["catalog"]["last_error"].is_null()
    })
    .await;
    std::fs::remove_file(h.paths.models_file()).unwrap();
    eventually("the delete", async || !h.has_model("extra/m1").await).await;
    assert_eq!(
        h.server.catalog_status().models,
        Catalog::builtin().models().count()
    );
}

/// The real daemon: SIGHUP reloads and does not end the process.
#[test]
fn sighup_reloads_the_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (addr, _mock) = rt.block_on(start_mock());
    let cfg = root.join("config/s1");
    std::fs::create_dir_all(&cfg).unwrap();
    // No watcher: only the SIGHUP can reload.
    std::fs::write(
        cfg.join("config.toml"),
        "watch_models = false\n[listen]\ntcp = \"127.0.0.1:0\"\n",
    )
    .unwrap();
    std::fs::write(cfg.join("models.toml"), overlay_for(addr)).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_s1"))
        .args(["util", "daemon", "run"])
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", root.join("home"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_RUNTIME_DIR", root.join("run"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let socket = root.join("run/s1.sock");
    let status = |socket: &Path| -> Option<Value> {
        let r = rt
            .block_on(request(
                socket,
                Method::GET,
                "/status",
                HeaderMap::new(),
                Bytes::new(),
            ))
            .ok()?;
        serde_json::from_slice(&r.body).ok()
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    while status(&socket).is_none() {
        assert!(Instant::now() < deadline, "daemon did not start");
        std::thread::sleep(Duration::from_millis(50));
    }

    // Bad file + SIGHUP: the reload reports the error, and the daemon lives
    // on (the default SIGHUP action would end it).
    std::fs::write(cfg.join("models.toml"), "[[[").unwrap();
    // SAFETY: kill(2) on the child we spawned.
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGHUP) }, 0);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let st = status(&socket).expect("daemon died on SIGHUP");
        if st["catalog"]["last_error"].is_string() {
            break;
        }
        assert!(Instant::now() < deadline, "SIGHUP did not reload: {st}");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        child.try_wait().unwrap().is_none(),
        "daemon exited on SIGHUP"
    );

    // SAFETY: as above.
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    assert!(child.wait().unwrap().success());
}
